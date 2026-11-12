//! A tiny word-level tokenizer and template stand in for a model snapshot,
//! so these run offline. `tests/live_glm.rs` checks a real model against
//! its server.

use super::*;
use serde_json::json;

/// Splits on whitespace only; every word in `vocab` is one token, `[UNK]` the rest.
fn tokenizer() -> Vec<u8> {
    let vocab: Map<String, Value> = [
        "[UNK]",
        "<|user|>",
        "<|assistant|>",
        "<|image|>",
        "<tool>",
        "</tool>",
    ]
    .iter()
    .enumerate()
    .map(|(id, word)| ((*word).to_owned(), json!(id)))
    .collect();
    serde_json::to_vec(&json!({
        "version": "1.0",
        "added_tokens": [],
        "pre_tokenizer": {"type": "WhitespaceSplit"},
        "model": {"type": "WordLevel", "vocab": vocab, "unk_token": "[UNK]"}
    }))
    .unwrap()
}

const TEMPLATE: &str = r#"
{%- for m in messages -%}
{%- if m.role == 'user' %} <|user|> {% if m.content is string %}{{ m.content }}{% else %}{% for p in m.content %}{% if p.type == 'text' %}{{ p.text }}{% else %} <|image|> {% endif %}{% endfor %}{% endif %}
{%- elif m.role == 'assistant' %} <|assistant|> {{ m.content }}{% for tc in m.tool_calls or [] %}{% set tc = tc.function %} <tool> {{ tc.name }}{% for k, v in tc.arguments.items() %} {{ k }} {{ v | tojson }}{% endfor %} </tool>{% endfor %}
{%- endif %}
{%- endfor %}
{%- if tools %} {{ tools | tojson }}{% endif %}
{%- if add_generation_prompt %} <|assistant|>{% endif %}"#;

fn counter(processor: Option<Value>) -> PromptCounter {
    PromptCounter::new(&tokenizer(), TEMPLATE, processor.as_ref()).unwrap()
}

#[test]
fn counts_the_rendered_template_with_arguments_as_objects() {
    let body = json!({"messages": [
        {"role": "user", "content": "list files"},
        {"role": "assistant", "content": "ok", "tool_calls": [{"id": "1", "type": "function",
            "function": {"name": "Bash", "arguments": "{\"command\": \"ls -la\"}"}}]}
    ]});
    // <|user|> list files <|assistant|> ok <tool> Bash command "ls -la" </tool> <|assistant|>
    // `"ls` and `-la"` are the two words of the argument's JSON.
    let count = counter(None).count_chat(&body).unwrap();
    assert_eq!(
        count,
        Count {
            prompt_tokens: 12,
            images: 0
        }
    );
}

#[test]
fn template_kwargs_and_tools_reach_the_template() {
    let plain = json!({"messages": [{"role": "user", "content": "hi"}]});
    let with_tools =
        json!({"messages": [{"role": "user", "content": "hi"}], "tools": [{"type": "function"}]});
    let counter = counter(None);
    let base = counter.count_chat(&plain).unwrap().prompt_tokens;
    assert!(counter.count_chat(&with_tools).unwrap().prompt_tokens > base);
}

#[test]
fn an_image_costs_its_patches_instead_of_its_placeholder() {
    // A 1x1 PNG: the GLM limits charge the minimum, 16 tokens.
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
    let processor = json!({"image_processor": {"patch_size": 14, "merge_size": 2, "min_image_tokens": 16, "max_image_tokens": 8000}});
    let body = json!({"messages": [{"role": "user", "content": [
        {"type": "text", "text": "what"},
        {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{png}")}}
    ]}]});
    let text_only =
        json!({"messages": [{"role": "user", "content": [{"type": "text", "text": "what"}]}]});
    let counter = counter(Some(processor));
    assert!(counter.accepts_images());
    let without = counter.count_chat(&text_only).unwrap().prompt_tokens;
    assert_eq!(
        counter.count_chat(&body).unwrap(),
        Count {
            prompt_tokens: without + 16,
            images: 1
        }
    );
}

#[test]
fn refuses_what_it_cannot_size() {
    let image = json!({"messages": [{"role": "user", "content": [{"type": "image_url", "image_url": {"url": "https://x/y.png"}}]}]});
    assert!(
        counter(None).count_chat(&image).is_err(),
        "no image processor"
    );
    let video = json!({"messages": [{"role": "user", "content": [{"type": "video_url", "video_url": {"url": "x"}}]}]});
    assert!(counter(None).count_chat(&video).is_err());
    assert!(counter(None).count_chat(&json!({"messages": []})).is_err());
}

#[test]
fn units_name_the_tokenizer_and_template() {
    let first = counter(None);
    assert!(first.units().starts_with("hf-tokenizer:"));
    assert_eq!(first.units(), counter(None).units());
    let other = PromptCounter::new(&tokenizer(), "{{ messages | length }}", None).unwrap();
    assert_ne!(first.units(), other.units());
}

#[test]
fn a_saved_truncation_or_padding_does_not_change_the_count() {
    let mut saved: Value = serde_json::from_slice(&tokenizer()).unwrap();
    saved["truncation"] =
        json!({"direction": "Right", "max_length": 2, "strategy": "LongestFirst", "stride": 0});
    saved["padding"] = json!({"strategy": {"Fixed": 64}, "direction": "Right", "pad_to_multiple_of": null, "pad_id": 0, "pad_type_id": 0, "pad_token": "[UNK]"});
    let saved = PromptCounter::new(&serde_json::to_vec(&saved).unwrap(), TEMPLATE, None).unwrap();
    let body = json!({"messages": [{"role": "user", "content": "one two three four five six"}]});
    assert_eq!(
        saved.count_chat(&body).unwrap(),
        counter(None).count_chat(&body).unwrap()
    );
}

#[test]
fn named_special_tokens_reach_the_template_and_tools_select_tool_use() {
    let templates = ChatTemplates {
        default: "{{ bos_token }} default".to_owned(),
        tool_use: Some("{{ bos_token }} tool use with tools".to_owned()),
    };
    let special = special_tokens(
        &json!({"bos_token": {"content": "<|user|>", "special": true}, "eos_token": "<|assistant|>", "model_max_length": 9}),
    );
    assert_eq!(special.get("bos_token"), Some(&json!("<|user|>")));
    assert!(!special.contains_key("model_max_length"));
    let counter = PromptCounter::build(&tokenizer(), &templates, None, special).unwrap();
    let plain = json!({"messages": [{"role": "user", "content": "x"}]});
    let tools =
        json!({"messages": [{"role": "user", "content": "x"}], "tools": [{"type": "function"}]});
    assert_eq!(counter.count_chat(&plain).unwrap().prompt_tokens, 2);
    assert_eq!(counter.count_chat(&tools).unwrap().prompt_tokens, 5);
}

#[test]
fn a_template_error_never_quotes_the_prompt() {
    let counter = PromptCounter::new(
        &tokenizer(),
        "{{ raise_exception(messages[0].content) }}",
        None,
    )
    .unwrap();
    let error = counter
        .count_chat(&json!({"messages": [{"role": "user", "content": "private-prompt-text"}]}))
        .unwrap_err();
    assert!(
        !format!("{error:#}").contains("private-prompt-text"),
        "{error:#}"
    );
}

#[test]
fn generation_tags_render_their_body_unchanged() {
    let template = "{%- for m in messages %}{%- generation -%} [{{ m.content }}] {%- endgeneration %}{% endfor %}";
    assert_eq!(
        without_generation_tags(template),
        "{%- for m in messages %}{%- if true -%} [{{ m.content }}] {%- endif %}{% endfor %}"
    );
    let counter = PromptCounter::new(&tokenizer(), template, None).unwrap();
    let body = json!({"messages": [{"role": "user", "content": "x"}]});
    assert!(counter.count_chat(&body).is_ok());
}

#[test]
fn templates_can_look_at_the_next_and_previous_message() {
    // Qwen's template closes a run of tool results by peeking ahead.
    let template = "{%- for m in messages %}{% if loop.previtem and loop.previtem.role != m.role %}|{% endif %}{{ m.role }}{% if not loop.last and loop.nextitem.role == 'tool' %}>{% endif %}{% endfor %}";
    let counter = PromptCounter::new(&tokenizer(), template, None).unwrap();
    let body = json!({"messages": [
        {"role": "user", "content": "x"},
        {"role": "assistant", "content": "y"},
        {"role": "tool", "content": "z"}
    ]});
    assert!(counter.count_chat(&body).is_ok());
}
