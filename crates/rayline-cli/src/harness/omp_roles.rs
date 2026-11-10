//! Hosted omp's saved model roles outlive the provider roster that seeded them.

use std::{
    io::{self, Read as _, Write as _},
    path::Path,
};

use super::{LaunchModels, omp_migration, yaml_merge};

/// Upgrade only the Rayline role choices the key does not list (the retired
/// Legacy, or an ARC id from before the key's data mode) in this environment's
/// saved omp settings.
pub(super) fn migrate(config_dir: &Path, models: &LaunchModels) -> io::Result<()> {
    let path = config_dir.join("config.yml");
    let _settings_lock = match super::omp_sessions::lock_settings(&path) {
        Ok(lock) => lock,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "cannot migrate hosted omp config.yml: settings lock unavailable; close the settings writer and retry: {error}"
                ),
            ));
        }
    };
    let Some(snapshot) = omp_migration::open(&path)? else {
        return Ok(());
    };
    let mut original = String::new();
    (&snapshot.file).read_to_string(&mut original)?;
    // Escaped double-quoted selectors can spell a Rayline selector without its
    // literal bytes, so only take the fast path when neither form can be present.
    if !original.contains("rayline/") && !original.contains('\\') {
        return Ok(());
    }
    let updated = yaml_merge::rewrite_model_roles(&original, |role, selector| {
        migrate_selector(role, selector, models)
    })
    .map_err(|error| {
        io::Error::new(io::ErrorKind::InvalidData, format!(
            "cannot migrate hosted omp config.yml model roles: {error}; edit unsupported settings syntax before launching again"
        ))
    })?;
    if updated == original {
        return Ok(());
    }
    omp_migration::replace(&path, &snapshot, |file| file.write_all(updated.as_bytes()))
}

fn migrate_selector(role: &str, selector: &str, models: &LaunchModels) -> Option<String> {
    let default = if role == "smol" {
        &models.background
    } else {
        &models.main
    };
    let mut updated: Option<String> = None;
    let mut copied = 0;
    let mut offset = 0;
    for part in selector.split(',') {
        let trimmed = part.trim();
        if let Some(model) = trimmed.strip_prefix("rayline/") {
            let (id, suffix) = model.find(':').map_or((model, ""), |at| model.split_at(at));
            if (suffix.is_empty()
                || matches!(
                    suffix,
                    ":off"
                        | ":minimal"
                        | ":low"
                        | ":medium"
                        | ":high"
                        | ":xhigh"
                        | ":max"
                        | ":auto"
                        | ":inherit"
                ))
                && let Some(replacement) = models.stale_pick(id, default)
            {
                // Replace the complete id, keeping its thinking suffix.
                let start = offset + part.len() - part.trim_start().len() + "rayline/".len();
                let end = start + id.len();
                let out = updated.get_or_insert_with(|| String::with_capacity(selector.len()));
                out.push_str(&selector[copied..start]);
                out.push_str(replacement);
                copied = end;
            }
        }
        offset += part.len() + 1;
    }
    if let Some(out) = &mut updated {
        out.push_str(&selector[copied..]);
    }
    updated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_settings_writer_blocks_migration_without_losing_its_edit() {
        let dir = super::super::tests::temp_dir("omp-roles-writer");
        let path = dir.join("config.yml");
        let original = "modelRoles:\n  default: rayline/rayline-router\n";
        std::fs::write(&path, original).unwrap();
        let writer = super::super::omp_sessions::lock_settings(&path).unwrap();
        assert!(migrate(&dir, &super::super::tests::hosted_models()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        let edited = "modelRoles:\n  default: rayline/rayline-router\n  slow: other/model\n";
        std::fs::write(&path, edited).unwrap();
        drop(writer);
        migrate(&dir, &super::super::tests::hosted_models()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "modelRoles:\n  default: rayline/arc-sage-1.0\n  slow: other/model\n"
        );
    }

    #[test]
    fn windows_line_endings_and_unicode_keep_correct_byte_spans() {
        let original = "\u{feff}# custom settings\r\nmodelRoles:\r\n  default: 'rayline/rayline-router'\r\n  smol: rayline/rayline-router\r\nname: café\r\n";
        let expected = "\u{feff}# custom settings\r\nmodelRoles:\r\n  default: 'rayline/arc-sage-1.0'\r\n  smol: rayline/arc-swift-1.0\r\nname: café\r\n";
        assert_eq!(rewrite(original).unwrap(), expected);
    }

    #[test]
    fn trailing_whitespace_and_missing_final_newline_survive() {
        let original =
            "modelRoles:\n  default: 'rayline/rayline-router'   \n  smol: rayline/rayline-router  ";
        let expected =
            "modelRoles:\n  default: 'rayline/arc-sage-1.0'   \n  smol: rayline/arc-swift-1.0  ";
        assert_eq!(rewrite(original).unwrap(), expected);
    }

    fn rewrite(input: &str) -> Result<String, String> {
        let models = super::super::tests::hosted_models();
        yaml_merge::rewrite_model_roles(input, |role, selector| {
            migrate_selector(role, selector, &models)
        })
    }

    #[test]
    fn unrelated_yaml_styles_are_preserved_byte_for_byte() {
        let before = "prompt: |\n  Please preserve 'quotes', {braces}, and modelRoles:\n  rayline/rayline-router\nflow: &settings {enabled: true, values: [one, two]}\ncopy: *settings\nmultilineFlow: [\n{modelRoles: not_a_root_role},\n\"literal [ brackets ]\"\n]\nquotedPrompt: \"first line\n  modelRoles: not_a_root_role\"\n";
        let roles = "modelRoles:\n  default: rayline/rayline-router # Legacy\n";
        let after = "tail: >\n  another prompt with 'unclosed quote\n  and a {brace\noptions: {custom: true}\n";
        let original = format!("{before}{roles}{after}");
        let expected =
            format!("{before}modelRoles:\n  default: rayline/arc-sage-1.0 # Legacy\n{after}");
        assert_eq!(rewrite(&original).unwrap(), expected);
        assert_eq!(rewrite(&expected).unwrap(), expected);
    }

    #[test]
    fn root_merge_or_duplicate_roles_are_ambiguous() {
        for original in [
            "base: &base {modelRoles: {default: other/model}}\n<<: *base\nmodelRoles:\n  default: rayline/rayline-router\n",
            "modelRoles:\n  default: rayline/rayline-router\nmodelRoles:\n  default: other/model\n",
        ] {
            assert!(rewrite(original).is_err());
        }
    }

    #[test]
    fn customized_roles_preserve_all_other_text() {
        let original = "# personal configuration\nmodelRoles: # saved choices\n  default: rayline/rayline-router # keep\n  smol: 'rayline/rayline-router'\n  slow: \"other/vendor-model\"\n  plan: rayline/arc-swift-1.0\n  custom: '@default'\ncustomPrompt: rayline/rayline-router\nother:\n  default: rayline/rayline-router\n";
        let expected = "# personal configuration\nmodelRoles: # saved choices\n  default: rayline/arc-sage-1.0 # keep\n  smol: 'rayline/arc-swift-1.0'\n  slow: \"other/vendor-model\"\n  plan: rayline/arc-swift-1.0\n  custom: '@default'\ncustomPrompt: rayline/rayline-router\nother:\n  default: rayline/rayline-router\n";
        assert_eq!(rewrite(original).unwrap(), expected);
        assert_eq!(rewrite(expected).unwrap(), expected);
    }

    #[test]
    fn quoted_keys_chains_and_scalar_sequences() {
        let original = "\u{feff}'modelRoles':\n  \"default\": \"other/model, rayline/rayline-router:high, @slow\" # chain\n  smol:\n    - 'rayline/rayline-router:minimal' # fast\n    - other/model\n  plan: rayline/rayline-router\n";
        let expected = "\u{feff}'modelRoles':\n  \"default\": \"other/model, rayline/arc-sage-1.0:high, @slow\" # chain\n  smol:\n    - 'rayline/arc-swift-1.0:minimal' # fast\n    - other/model\n  plan: rayline/arc-sage-1.0\n";
        assert_eq!(rewrite(original).unwrap(), expected);
        assert_eq!(rewrite(expected).unwrap(), expected);
    }

    #[test]
    fn listed_ids_and_other_providers_are_kept() {
        let original = "modelRoles:\n  default: other/rayline-router\n  slow: rayline/arc-swift-1.0:high\n  smol: rayline/arc-sage-1.0\n";
        assert_eq!(rewrite(original).unwrap(), original);
        assert_eq!(rewrite("# no roles\n{}").unwrap(), "# no roles\n{}");
        assert_eq!(rewrite("modelRoles: {}\n").unwrap(), "modelRoles: {}\n");
    }

    #[test]
    fn unsupported_and_ambiguous_syntax_is_rejected_without_contents() {
        for original in [
            "modelRoles: {default: secret}\n",
            "modelRoles:\n  default: [secret]\n",
            "modelRoles:\n  default: secret\n  default: rayline/rayline-router\n",
            "modelRoles:\n  default:\n    model: secret\n",
            "modelRoles:\n  default: \"secret\\x20\"\n",
        ] {
            let error = rewrite(original).unwrap_err();
            assert!(!error.contains("secret"));
        }
    }

    #[test]
    fn ids_the_key_does_not_list_move_to_its_listed_models() {
        let models = super::super::LaunchModels::from_listing(&super::super::tests::listing(Some(
            "contributor",
        )))
        .unwrap();
        let original = "modelRoles:\n  default: rayline/arc-sage-1.0:high\n  smol: rayline/arc-swift-1.0\n  plan: rayline/rayline-router\n  slow: other/arc-sage-1.0\n  task: rayline/arc-swift-contributor-1.0\n";
        let expected = "modelRoles:\n  default: rayline/arc-sage-contributor-1.0:high\n  smol: rayline/arc-swift-contributor-1.0\n  plan: rayline/arc-sage-contributor-1.0\n  slow: other/arc-sage-1.0\n  task: rayline/arc-swift-contributor-1.0\n";
        let rewrite = |input: &str| {
            yaml_merge::rewrite_model_roles(input, |role, selector| {
                migrate_selector(role, selector, &models)
            })
            .unwrap()
        };
        assert_eq!(rewrite(original), expected);
        assert_eq!(rewrite(expected), expected);
    }

    #[test]
    fn absent_config_is_untouched() {
        let dir = std::env::temp_dir().join(format!(
            "rayline-omp-absent-{}",
            super::super::new_conversation_id()
        ));
        migrate(&dir, &super::super::tests::hosted_models()).unwrap();
        assert!(!dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn file_migration_is_private_idempotent_and_refuses_symlinks() {
        use std::{
            fs,
            os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink},
        };
        let dir = std::env::temp_dir().join(format!(
            "rayline-omp-roles-{}",
            super::super::new_conversation_id()
        ));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("config.yml");
        fs::write(&path, "modelRoles:\n  default: rayline/rayline-router\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        migrate(&dir, &super::super::tests::hosted_models()).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "modelRoles:\n  default: rayline/arc-sage-1.0\n"
        );
        let metadata = fs::metadata(&path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        migrate(&dir, &super::super::tests::hosted_models()).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().ino(), metadata.ino());
        fs::rename(&path, dir.join("target")).unwrap();
        symlink("target", &path).unwrap();
        assert!(migrate(&dir, &super::super::tests::hosted_models()).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
