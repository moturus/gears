//! Atomic UTF-8 replacements authorized by complete reads of this version.

use std::path::Path;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::StateRoot;
use crate::tools::Context;
use crate::tools::record::{FileVersion, fully_read, record};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteFile {
    path: String,
    content: String,
}

pub fn execute(arguments: &str, context: &Context) -> Result<Value, String> {
    let arguments: WriteFile = serde_json::from_str(arguments)
        .map_err(|error| format!("invalid write_file arguments: {error}"))?;
    write(context, &arguments.path, &arguments.content)
}

fn write(context: &Context, input: &str, content: &str) -> Result<Value, String> {
    let path = context.resolve(input, true)?;
    let previous = match std::fs::symlink_metadata(&path) {
        Ok(_) => Some(crate::tools::read_file::version(&path)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.to_string()),
    };
    write_over(context, &path, previous, content)
}

/// Replace `path`, whose current version the caller has already determined.
pub(crate) fn write_over(
    context: &Context,
    path: &Path,
    previous: Option<FileVersion>,
    content: &str,
) -> Result<Value, String> {
    if let Some(version) = &previous
        && !fully_read(context, path, version)?
    {
        return Err(format!(
            "refusing to write {}: it has not been read completely in this session, or it \
             changed since it was read. Call read_file and follow next_offset until \
             all_pages_read is true, then retry.",
            context.relative(path).display()
        ));
    }
    replace(context, path, previous, content)
}

/// Replace `path` without requiring a prior read. `patch_file` keeps every
/// byte outside its unique match, so it needs no read coverage.
pub(crate) fn replace(
    context: &Context,
    path: &Path,
    previous: Option<FileVersion>,
    content: &str,
) -> Result<Value, String> {
    if content.contains('\0') {
        return Err("binary files are not supported".into());
    }
    let mut content = match &previous {
        Some(version) => {
            let text = content.strip_prefix('\u{feff}').unwrap_or(content);
            if version.bom {
                format!("\u{feff}{text}")
            } else {
                text.into()
            }
        }
        None => content.to_string(),
    };
    if previous
        .as_ref()
        .is_some_and(|version| version.windows_lines)
    {
        content = content.replace("\r\n", "\n").replace('\n', "\r\n");
    }
    StateRoot::new(Path::new("/").to_path_buf())?
        .replace_file_preserving_access(path.strip_prefix("/").unwrap(), content.as_bytes())?;
    let version = FileVersion::of(content.as_bytes());
    let mut receipt = json!({"path": context.relative(path), "hash": version.hash,
        "total_bytes": version.size, "written": true});
    // A bookkeeping failure must not misreport a workspace write as refused.
    if let Err(error) = record(context, path, &version, 0, version.size) {
        receipt["warning"] = json!(format!("file was written, but its record failed: {error}"));
    }
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> (std::path::PathBuf, Context) {
        let root =
            std::env::temp_dir().join(format!("gears-tools-write-{name}-{}", std::process::id()));
        std::fs::create_dir_all(root.join("workspace")).unwrap();
        let context = Context::new(
            root.join("workspace"),
            root.join("state"),
            "s1".into(),
            None,
        )
        .unwrap();
        (root, context)
    }

    #[test]
    fn incomplete_and_changed_reads_refuse_overwrites_but_own_writes_remain_editable() {
        let (root, context) = fixture("coverage");
        let path = context.workspace.join("text");
        std::fs::write(&path, "a".repeat(40_000)).unwrap();
        let refused = write(&context, "text", "replacement").unwrap_err();
        assert!(refused.contains("refusing to write text"), "{refused}");
        assert!(refused.contains("read_file"), "{refused}");
        let first = crate::tools::read_file::execute(r#"{"path":"text"}"#, &context).unwrap();
        assert!(write(&context, "text", "replacement").is_err());
        let next = first["next_offset"].as_u64().unwrap();
        crate::tools::read_file::execute(
            &serde_json::json!({"path": "text", "offset": next}).to_string(),
            &context,
        )
        .unwrap();
        std::fs::write(&path, "b".repeat(40_000)).unwrap();
        assert!(write(&context, "text", "replacement").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "b".repeat(40_000));
        write(&context, "new/dir/file", "first").unwrap();
        assert_eq!(
            write(&context, "new/dir/file", "second").unwrap()["written"],
            true
        );
        assert_eq!(
            std::fs::read_to_string(context.workspace.join("new/dir/file")).unwrap(),
            "second"
        );
        assert!(write(&context, "new/binary", "\0").is_err());
        assert!(!context.workspace.join("new/binary").exists());
        assert!(
            write(
                &context,
                context.state.path().join("key").to_str().unwrap(),
                "bad"
            )
            .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn keeps_bom_and_windows_lines() {
        let (root, context) = fixture("format");
        let path = context.workspace.join("script");
        std::fs::write(&path, "\u{feff}first\r\nsecond\r\n").unwrap();
        crate::tools::read_file::execute(r#"{"path":"script"}"#, &context).unwrap();
        write(&context, "script", "changed\nline\r\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "\u{feff}changed\r\nline\r\n"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reports_a_write_even_if_later_bookkeeping_fails() {
        let (root, context) = fixture("receipt");
        std::fs::create_dir_all(root.join("state/tools/sessions/s1")).unwrap();
        std::fs::write(root.join("state/tools/sessions/s1/reads"), "obstruction").unwrap();
        let result = write(&context, "created", "actual contents").unwrap();
        assert_eq!(result["written"], true);
        assert!(
            result["warning"]
                .as_str()
                .unwrap()
                .contains("file was written")
        );
        assert_eq!(
            std::fs::read_to_string(context.workspace.join("created")).unwrap(),
            "actual contents"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
