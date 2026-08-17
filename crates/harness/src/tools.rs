use std::{
    error::Error,
    fmt,
    future::Future,
    io::{self, Write},
    path::{Component, Path, PathBuf},
};

use nexa_protocol::{ToolCall, ToolDefinition, ToolResult};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use tokio::fs;

use crate::ToolBridge;

const MAX_READ_BYTES: u64 = 1024 * 1024;

pub struct WorkspaceTools {
    root: PathBuf,
}

impl WorkspaceTools {
    pub fn new(root: impl AsRef<Path>) -> Result<Self, WorkspaceToolsError> {
        let root = std::fs::canonicalize(root).map_err(WorkspaceToolsError::Root)?;
        if !root.is_dir() {
            return Err(WorkspaceToolsError::Root(io::Error::new(
                io::ErrorKind::InvalidInput,
                "workspace root is not a directory",
            )));
        }
        Ok(Self { root })
    }

    async fn execute_call(&self, call: ToolCall) -> ToolResult {
        let outcome = match call.name.as_str() {
            "read_file" => self.read_file(&call.arguments).await,
            "edit_file" => self.edit_file(&call.arguments).await,
            _ => Err(format!("unknown tool {:?}", call.name)),
        };

        match outcome {
            Ok(content) => ToolResult {
                tool_call_id: call.id,
                content,
                is_error: false,
            },
            Err(error) => ToolResult {
                tool_call_id: call.id,
                content: json!({ "error": error }).to_string(),
                is_error: true,
            },
        }
    }

    async fn read_file(&self, arguments: &str) -> Result<String, String> {
        let arguments: ReadFileArguments = parse_arguments(arguments, "read_file")?;
        let path = self.resolve_existing_file(&arguments.path).await?;
        let metadata = fs::metadata(&path)
            .await
            .map_err(|error| format!("could not inspect {:?}: {error}", arguments.path))?;
        if metadata.len() > MAX_READ_BYTES {
            return Err(format!(
                "file {:?} is {} bytes; the read_file limit is {MAX_READ_BYTES} bytes",
                arguments.path,
                metadata.len()
            ));
        }
        let content = fs::read_to_string(path).await.map_err(|error| {
            format!("could not read {:?} as UTF-8 text: {error}", arguments.path)
        })?;
        Ok(json!({
            "path": arguments.path,
            "content": content,
        })
        .to_string())
    }

    async fn edit_file(&self, arguments: &str) -> Result<String, String> {
        let arguments: EditFileArguments = parse_arguments(arguments, "edit_file")?;
        if arguments.old_text.is_empty() {
            return self.create_file(arguments).await;
        }
        let path = self.resolve_existing_file(&arguments.path).await?;
        let content = fs::read_to_string(&path).await.map_err(|error| {
            format!("could not read {:?} as UTF-8 text: {error}", arguments.path)
        })?;
        let matches = content.match_indices(&arguments.old_text).count();
        match matches {
            0 => return Err("edit_file old_text did not match the file".to_owned()),
            1 => {}
            count => {
                return Err(format!(
                    "edit_file old_text matched {count} locations; provide more context"
                ));
            }
        }

        let updated = content.replacen(&arguments.old_text, &arguments.new_text, 1);
        let before_sha256 = sha256(&content);
        let after_sha256 = sha256(&updated);
        let write_path = path.clone();
        tokio::task::spawn_blocking(move || atomic_replace(&write_path, updated))
            .await
            .map_err(|error| format!("edit_file write task failed: {error}"))?
            .map_err(|error| format!("could not update {:?}: {error}", arguments.path))?;

        Ok(json!({
            "path": arguments.path,
            "beforeSha256": before_sha256,
            "afterSha256": after_sha256,
        })
        .to_string())
    }

    async fn create_file(&self, arguments: EditFileArguments) -> Result<String, String> {
        let requested_path = validate_relative_path(&arguments.path)?;
        let path = self.root.join(requested_path);
        match fs::symlink_metadata(&path).await {
            Ok(_) => {
                return Err(
                    "edit_file old_text may only be empty when creating a missing file".to_owned(),
                );
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "could not inspect {:?} before creating it: {error}",
                    arguments.path
                ));
            }
        }

        let parent = path
            .parent()
            .ok_or_else(|| "new file has no parent directory".to_owned())?;
        let canonical_parent = fs::canonicalize(parent).await.map_err(|error| {
            format!(
                "could not resolve the parent directory for {:?}: {error}",
                arguments.path
            )
        })?;
        if !canonical_parent.starts_with(&self.root) {
            return Err(format!(
                "path {:?} resolves outside the workspace",
                arguments.path
            ));
        }

        let after_sha256 = sha256(&arguments.new_text);
        let file_name = path
            .file_name()
            .ok_or_else(|| "new file has no file name".to_owned())?;
        let write_path = canonical_parent.join(file_name);
        tokio::task::spawn_blocking(move || atomic_create(&write_path, arguments.new_text))
            .await
            .map_err(|error| format!("edit_file write task failed: {error}"))?
            .map_err(|error| format!("could not create {:?}: {error}", arguments.path))?;

        Ok(json!({
            "path": arguments.path,
            "created": true,
            "afterSha256": after_sha256,
        })
        .to_string())
    }

    async fn resolve_existing_file(&self, requested: &str) -> Result<PathBuf, String> {
        let requested_path = validate_relative_path(requested)?;

        let canonical = fs::canonicalize(self.root.join(requested_path))
            .await
            .map_err(|error| format!("could not resolve {requested:?}: {error}"))?;
        if !canonical.starts_with(&self.root) {
            return Err(format!("path {requested:?} resolves outside the workspace"));
        }
        let metadata = fs::metadata(&canonical)
            .await
            .map_err(|error| format!("could not inspect {requested:?}: {error}"))?;
        if !metadata.is_file() {
            return Err(format!("path {requested:?} is not a file"));
        }
        Ok(canonical)
    }
}

impl ToolBridge for WorkspaceTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "read_file".to_owned(),
                description: "Read one UTF-8 text file inside the workspace.".to_owned(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path relative to the workspace root."
                        }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
            },
            ToolDefinition {
                name: "edit_file".to_owned(),
                description: "Create a missing workspace file when old_text is empty, or replace exactly one matching text fragment in an existing file. Include enough surrounding text to make old_text unique.".to_owned(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path relative to the workspace root."
                        },
                        "old_text": {
                            "type": "string",
                            "description": "Exact text to replace. It must occur exactly once in an existing file. Use an empty string only to create a missing file."
                        },
                        "new_text": {
                            "type": "string",
                            "description": "Replacement text."
                        }
                    },
                    "required": ["path", "old_text", "new_text"],
                    "additionalProperties": false
                }),
            },
        ]
    }

    fn execute(&self, call: ToolCall) -> impl Future<Output = ToolResult> + Send {
        self.execute_call(call)
    }
}

fn validate_relative_path(requested: &str) -> Result<&Path, String> {
    let requested_path = Path::new(requested);
    if requested_path.as_os_str().is_empty()
        || requested_path.is_absolute()
        || requested_path
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(
            "tool paths must be relative to the workspace and may not contain ..".to_owned(),
        );
    }
    Ok(requested_path)
}

fn parse_arguments<T>(arguments: &str, tool_name: &str) -> Result<T, String>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(arguments)
        .map_err(|error| format!("invalid {tool_name} arguments: {error}"))
}

fn atomic_replace(path: &Path, content: String) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("file has no parent directory"))?;
    let permissions = std::fs::metadata(path)?.permissions();
    let mut temporary = NamedTempFile::new_in(parent)?;
    temporary.as_file().set_permissions(permissions)?;
    temporary.write_all(content.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn atomic_create(path: &Path, content: String) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("file has no parent directory"))?;
    let mut temporary = NamedTempFile::new_in(parent)?;
    temporary.write_all(content.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary
        .persist_noclobber(path)
        .map_err(|error| error.error)?;
    Ok(())
}

fn sha256(content: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(content.as_bytes());
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadFileArguments {
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditFileArguments {
    path: String,
    old_text: String,
    new_text: String,
}

#[derive(Debug)]
pub enum WorkspaceToolsError {
    Root(io::Error),
}

impl fmt::Display for WorkspaceToolsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Root(error) => write!(formatter, "invalid workspace root: {error}"),
        }
    }
}

impl Error for WorkspaceToolsError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Root(error) => Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use nexa_protocol::ToolCall;
    use tempfile::tempdir;

    use super::{ToolBridge, WorkspaceTools};

    #[tokio::test]
    async fn reads_and_edits_one_exact_match() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("notes.txt"), "alpha\nbeta\n").unwrap();
        let tools = WorkspaceTools::new(directory.path()).unwrap();

        let read = tools
            .execute(ToolCall {
                id: "read-1".to_owned(),
                name: "read_file".to_owned(),
                arguments: r#"{"path":"notes.txt"}"#.to_owned(),
            })
            .await;
        assert!(!read.is_error);
        assert!(read.content.contains("alpha\\nbeta\\n"));

        let edit = tools
            .execute(ToolCall {
                id: "edit-1".to_owned(),
                name: "edit_file".to_owned(),
                arguments: r#"{"path":"notes.txt","old_text":"beta","new_text":"gamma"}"#
                    .to_owned(),
            })
            .await;
        assert!(!edit.is_error);
        assert_eq!(
            fs::read_to_string(directory.path().join("notes.txt")).unwrap(),
            "alpha\ngamma\n"
        );
    }

    #[tokio::test]
    async fn creates_a_missing_file_from_an_empty_old_text() {
        let directory = tempdir().unwrap();
        let tools = WorkspaceTools::new(directory.path()).unwrap();

        let create = tools
            .execute(ToolCall {
                id: "edit-1".to_owned(),
                name: "edit_file".to_owned(),
                arguments: r#"{"path":"hello.md","old_text":"","new_text":"hello\n"}"#.to_owned(),
            })
            .await;

        assert!(!create.is_error);
        assert!(create.content.contains(r#""created":true"#));
        assert_eq!(
            fs::read_to_string(directory.path().join("hello.md")).unwrap(),
            "hello\n"
        );

        let overwrite = tools
            .execute(ToolCall {
                id: "edit-2".to_owned(),
                name: "edit_file".to_owned(),
                arguments: r#"{"path":"hello.md","old_text":"","new_text":"replaced\n"}"#
                    .to_owned(),
            })
            .await;
        assert!(overwrite.is_error);
        assert!(overwrite.content.contains("only be empty"));
        assert_eq!(
            fs::read_to_string(directory.path().join("hello.md")).unwrap(),
            "hello\n"
        );
    }

    #[tokio::test]
    async fn rejects_ambiguous_edits_and_workspace_escapes() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("notes.txt"), "same same").unwrap();
        let tools = WorkspaceTools::new(directory.path()).unwrap();

        let ambiguous = tools
            .execute(ToolCall {
                id: "edit-1".to_owned(),
                name: "edit_file".to_owned(),
                arguments: r#"{"path":"notes.txt","old_text":"same","new_text":"other"}"#
                    .to_owned(),
            })
            .await;
        assert!(ambiguous.is_error);
        assert!(ambiguous.content.contains("matched 2 locations"));

        let escaped = tools
            .execute(ToolCall {
                id: "read-1".to_owned(),
                name: "read_file".to_owned(),
                arguments: r#"{"path":"../outside.txt"}"#.to_owned(),
            })
            .await;
        assert!(escaped.is_error);
        assert!(escaped.content.contains("relative to the workspace"));
    }
}
