//! The HTML served by the loopback listener that catches the `raydock auth
//! login` OAuth redirect.
//!
//! This is the only Rayline surface rendered by the CLI itself rather than by
//! the site, so it is a hand-maintained port of the platform sign-in screen.
//! Sources of truth, in the `memex-desktop` repo:
//!
//! - `turbo/apps/rayline/src/app.css` — palette, type, and theme tokens.
//! - `turbo/packages/ui/components/auth/AuthPageShell.svelte` — the card.
//! - `turbo/packages/ui/components/auth/AuthHeroBackground.svelte` — backdrop.
//!
//! Two deliberate departures from those files: the backdrop is static (the
//! site's is an animated SVG driven by Svelte, which a one-shot page does not
//! need), and the card's paper grid uses a slightly stronger line in dark mode,
//! because the site's `--color-border-subtle` is the card background there and
//! the texture disappears. Fonts load from the site's CDN with a system stack
//! behind them, so the page still renders correctly offline.

/// A login-callback page rendered in the user's browser after the OAuth round
/// trip. `body` is inserted as raw HTML, so callers must escape any untrusted
/// content (e.g. error messages) before passing it in.
struct CallbackPage<'a> {
    /// Used for the document `<title>` (kept short).
    doc_title: &'a str,
    /// The on-page heading.
    heading: &'a str,
    /// The supporting paragraph (raw HTML; pre-escape untrusted input).
    body: &'a str,
    is_error: bool,
}

pub(crate) fn success_html() -> String {
    render(&CallbackPage {
        doc_title: "Logged in",
        heading: "Logged in",
        body: "You can close this tab and return to the terminal.",
        is_error: false,
    })
}

pub(crate) fn waiting_html() -> String {
    render(&CallbackPage {
        doc_title: "Waiting",
        heading: "Waiting for sign-in",
        body: "Complete sign-in in the browser tab opened by the CLI.",
        is_error: false,
    })
}

pub(crate) fn error_html(message: &str) -> String {
    render(&CallbackPage {
        doc_title: "Login failed",
        heading: "Login failed",
        body: &html_escape(message),
        is_error: true,
    })
}

fn render(page: &CallbackPage) -> String {
    let heading_class = if page.is_error {
        "title title--error"
    } else {
        "title"
    };
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="dark light">
<meta name="theme-color" content="#0a0d10" media="(prefers-color-scheme: dark)">
<meta name="theme-color" content="#f0eee6" media="(prefers-color-scheme: light)">
<title>{doc_title} — {brand}</title>
<style>{styles}</style>
</head>
<body>
<div class="backdrop" aria-hidden="true">
<div class="aurora aurora--ember"></div>
<div class="aurora aurora--ink"></div>
<div class="field"></div>
<div class="vignette"></div>
</div>
<main class="card">
<div class="logo">{logo}</div>
<h1 class="{heading_class}">{heading}</h1>
<p class="subtitle">{body}</p>
</main>
</body>
</html>"##,
        doc_title = page.doc_title,
        brand = crate::DISPLAY_NAME,
        styles = PAGE_STYLES,
        logo = WORDMARK_SVG,
        heading_class = heading_class,
        heading = page.heading,
        body = page.body,
    )
}

/// Bone Mist / Black neutrals with the Solar Ember accent. Dark is the base
/// because the site treats it as the default and only switches on an explicit
/// `prefers-color-scheme: light`.
const PAGE_STYLES: &str = r##"
@font-face{
  font-family:"Akkurat";
  src:url("https://static.workshop.ai/rayline/site/v2026-08-01/fonts/Akkurat-Regular-0db55ef2a5.woff2") format("woff2");
  font-weight:400;
  font-style:normal;
  font-display:swap;
}
@font-face{
  font-family:"Sohne";
  src:url("https://static.workshop.ai/rayline/site/v2026-08-01/fonts/Sohne-Kraftig-ae95091d5e.woff2") format("woff2");
  font-weight:500;
  font-style:normal;
  font-display:swap;
}
@font-face{
  font-family:"Sohne";
  src:url("https://static.workshop.ai/rayline/site/v2026-08-01/fonts/Sohne-Halbfett-9f1241e1ab.woff2") format("woff2");
  font-weight:600;
  font-style:normal;
  font-display:swap;
}
:root{
  color-scheme:dark;
  --surface:#0a0d10;
  --surface-rgb:10 13 16;
  --card:#111418;
  --ink:#f0eee6;
  --ink-rgb:240 238 230;
  --muted:#9a9283;
  --line:#252a30;
  --card-grid:#181c21;
  --ember:#ea5e2a;
  --ember-rgb:234 94 42;
  --danger:#fca5a5;
  --field-line:rgb(240 238 230 / 0.045);
  --shadow:rgb(0 0 0 / 0.42);
  --aurora-opacity:0.78;
  --aurora-ink-alpha:0.14;
}
@media (prefers-color-scheme:light){
  :root{
    color-scheme:light;
    --surface:#f0eee6;
    --surface-rgb:240 238 230;
    --card:#ffffff;
    --ink:#0a0d10;
    --ink-rgb:10 13 16;
    --muted:#3b424a;
    --line:#d9d5c8;
    --card-grid:#f0eee6;
    --danger:#dc2626;
    --field-line:rgb(10 13 16 / 0.055);
    --shadow:rgb(10 13 16 / 0.12);
    --aurora-opacity:0.24;
    --aurora-ink-alpha:0.24;
  }
}
*{box-sizing:border-box}
html{height:100%}
body{
  margin:0;
  min-height:100vh;
  min-height:100dvh;
  display:flex;
  align-items:center;
  justify-content:center;
  padding:1.25rem;
  overflow:hidden;
  background:var(--surface);
  color:var(--ink);
  font-family:"Akkurat",Inter,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;
  -webkit-font-smoothing:antialiased;
  -moz-osx-font-smoothing:grayscale;
}
.backdrop{
  position:fixed;
  inset:0;
  z-index:0;
  overflow:hidden;
  pointer-events:none;
}
.aurora{
  position:absolute;
  border-radius:9999px;
  filter:blur(78px);
  opacity:var(--aurora-opacity);
}
.aurora--ember{
  top:8rem;
  left:max(-10rem,-10vw);
  width:24rem;
  height:24rem;
  background:radial-gradient(circle,rgb(var(--ember-rgb) / 0.2),transparent 68%);
}
.aurora--ink{
  top:14rem;
  right:max(-12rem,-12vw);
  width:28rem;
  height:28rem;
  background:radial-gradient(circle,rgb(var(--ink-rgb) / var(--aurora-ink-alpha)),transparent 70%);
}
.field{
  position:absolute;
  inset:0;
  background-image:
    linear-gradient(var(--field-line) 1px,transparent 1px),
    linear-gradient(90deg,var(--field-line) 1px,transparent 1px);
  background-size:120px 120px;
  -webkit-mask-image:radial-gradient(ellipse 80% 70% at 50% 45%,#000 30%,transparent 100%);
  mask-image:radial-gradient(ellipse 80% 70% at 50% 45%,#000 30%,transparent 100%);
}
.vignette{
  position:absolute;
  inset:0;
  background:
    radial-gradient(ellipse 50% 42% at 50% 38%,transparent 0%,rgb(var(--surface-rgb) / 0.86) 76%),
    linear-gradient(180deg,transparent,rgb(var(--surface-rgb) / 0.84));
}
.card{
  position:relative;
  z-index:1;
  width:min(100%,27rem);
  overflow:hidden;
  border:1px solid var(--line);
  border-radius:1.35rem;
  background:var(--card);
  padding:clamp(1.25rem,5vw,2.1rem);
  box-shadow:0 30px 82px var(--shadow);
  text-align:center;
}
/* Faint paper grid, the same texture the marketing pages carry. */
.card::before{
  content:"";
  position:absolute;
  inset:0;
  background-image:
    linear-gradient(var(--card-grid) 1px,transparent 1px),
    linear-gradient(90deg,var(--card-grid) 1px,transparent 1px);
  background-size:2rem 2rem;
  opacity:0.45;
  pointer-events:none;
}
/* A single accent hairline across the top of the card. */
.card::after{
  content:"";
  position:absolute;
  top:0;
  right:2rem;
  left:2rem;
  height:1px;
  background:linear-gradient(90deg,transparent,var(--ember),transparent);
  opacity:0.55;
  pointer-events:none;
}
.logo,.title,.subtitle{position:relative;z-index:1}
.logo{
  display:flex;
  justify-content:center;
  margin:0 0 0.85rem;
}
.logo svg{height:2rem;width:auto}
.title{
  margin:0 0 0.4rem;
  font-family:"Sohne","Akkurat",system-ui,sans-serif;
  font-size:clamp(1.55rem,6vw,2.05rem);
  font-weight:500;
  letter-spacing:-0.055em;
  line-height:1.04;
}
.title--error{color:var(--danger)}
.subtitle{
  margin:0;
  color:var(--muted);
  font-size:0.9rem;
  line-height:1.5;
}
@media (max-width:520px){
  body{padding:0.9rem}
  .card{border-radius:1.15rem}
}
"##;

/// The horizontal Rayline lockup, from the brand CDN
/// (`brand/rayline/rayline-horizontal.db66b1480b88.svg`), inlined so the page
/// renders without network access and recolored to `currentColor` so one copy
/// serves both themes. The `stroke="black"` below is inside an alpha mask and
/// is opacity, not color — leave it.
const WORDMARK_SVG: &str = r##"<svg width="1000" height="333" role="img" aria-label="Rayline" viewBox="0 0 1000 333" fill="none" xmlns="http://www.w3.org/2000/svg">
<rect y="14.3438" width="137.671" height="60.6327" fill="currentColor"/>
<rect y="128.726" width="71.7037" height="140.539" fill="currentColor"/>
<path d="M2.86816 188.957H84.6104L162.05 267.831H259.567L123.33 128.726H2.86816V188.957Z" fill="currentColor"/>
<mask id="rayline-mark-mask" style="mask-type:alpha" maskUnits="userSpaceOnUse" x="34" y="14" width="190" height="173">
<path d="M129.066 44.1152C167.473 44.1152 193.601 71.793 193.601 100.045C193.6 128.297 167.472 155.974 129.066 155.974C90.6608 155.973 64.5335 128.297 64.5332 100.045C64.5332 71.7931 90.6606 44.1155 129.066 44.1152Z" stroke="black" stroke-width="60.2311"/>
</mask>
<g mask="url(#rayline-mark-mask)">
<rect width="120.462" height="97.517" transform="matrix(-1 0 0 1 258.135 14)" fill="currentColor"/>
<rect width="95.7553" height="177.825" transform="matrix(-0.709545 0.70466 0.70466 0.709545 204.813 46.8633)" fill="currentColor"/>
</g>
<path d="M314.797 270.263C289.65 270.263 270.287 254.924 270.287 231.285C270.287 206.138 289.147 195.074 314.546 189.793L351.009 182.249V179.985C351.009 167.412 344.471 159.616 328.376 159.616C314.043 159.616 306.499 166.154 302.978 178.979L274.562 172.441C281.1 150.563 300.463 133.463 329.634 133.463C361.319 133.463 380.431 148.551 380.431 178.979V235.812C380.431 243.356 383.7 245.619 391.999 244.613V268C370.121 270.515 358.553 266.24 354.026 255.426C345.728 264.731 331.897 270.263 314.797 270.263ZM351.009 221.478V205.384L322.593 211.419C309.768 214.185 300.212 218.209 300.212 230.028C300.212 240.338 307.756 246.122 319.324 246.122C335.418 246.122 351.009 237.572 351.009 221.478ZM483.404 280.322C475.86 300.44 464.292 314.271 436.882 314.271C430.595 314.271 428.835 314.019 424.56 313.516V288.118C428.584 288.621 430.847 288.872 434.619 288.872C444.678 288.872 449.456 286.106 453.731 275.544L458.76 263.222L410.729 135.978H442.414L474.854 228.519L506.539 135.978H537.722L483.404 280.322ZM599.279 87.4441V268H569.103V87.4441H599.279ZM645.573 118.375V87.4441H676.756V118.375H645.573ZM676.253 135.978V268H646.076V135.978H676.253ZM723.05 268V135.978H753.226V150.06C760.77 141.259 772.59 133.463 789.69 133.463C817.351 133.463 833.948 152.575 833.948 180.991V268H803.772V189.793C803.772 173.447 797.234 161.628 780.637 161.628C767.057 161.628 753.226 171.687 753.226 190.547V268H723.05ZM935.662 270.766C897.942 270.766 871.537 242.853 871.537 202.115C871.537 163.388 897.69 133.463 934.405 133.463C972.628 133.463 992.998 162.382 992.998 198.594V208.653H900.456C902.72 231.285 916.299 245.116 935.662 245.116C950.499 245.116 962.318 237.572 966.342 223.993L992.243 233.8C982.939 256.935 962.067 270.766 935.662 270.766ZM934.153 158.862C918.562 158.862 906.492 168.166 901.965 186.021H962.57C962.318 171.435 953.265 158.862 934.153 158.862Z" fill="currentColor"/>
</svg>"##;

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_page_carries_the_brand_shell() {
        let html = success_html();
        assert!(html.contains("<title>Logged in — Raydock</title>"));
        assert!(html.contains("You can close this tab and return to the terminal."));
        assert!(html.contains("aria-label=\"Rayline\""));
        assert!(html.contains("--ember:#ea5e2a"));
        assert!(html.contains("class=\"title\""));
    }

    #[test]
    fn error_page_escapes_the_message_and_marks_the_heading() {
        let html = error_html("bad <script>\"state\" & code</script>");
        assert!(html.contains("class=\"title title--error\""));
        assert!(html.contains("bad &lt;script&gt;&quot;state&quot; &amp; code&lt;/script&gt;"));
        assert!(!html.contains("<script>"));
    }

    #[test]
    fn waiting_page_asks_the_user_to_finish_in_the_other_tab() {
        let html = waiting_html();
        assert!(html.contains("Waiting for sign-in"));
        assert!(html.contains("Complete sign-in in the browser tab opened by the CLI."));
    }
}
