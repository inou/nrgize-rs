pub mod bash_parser;
pub mod env_parser;
pub mod models;
pub mod starlark_parser;

use models::ParseResult;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ParseError {
    #[error("No task file found in current directory")]
    NoFileFound,

    #[error("File not found: {0}")]
    FileNotFound(PathBuf),

    #[error("Failed to read file {path}: {source}")]
    IoError {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("Parse error in {file} at line {line}: {message}")]
    #[allow(dead_code)]
    SyntaxError {
        file: PathBuf,
        line: usize,
        message: String,
    },

    #[error("Unknown file format: {0}")]
    UnknownFormat(String),

    #[error("{0}")]
    Other(String),
}

/// Trait for task file parsers.
pub trait Parser {
    fn parse(&self, path: &Path, content: &str) -> Result<ParseResult, ParseError>;
}

/// File search order for automatic resolution.
const FILE_SEARCH_ORDER: &[&str] = &["Energize.star", "energize.star", "Energize.sh", "energize.sh"];

/// Resolve the task file path.
/// If `path` is given, use it directly.
/// If `conf` is given, look for that filename in CWD.
/// Otherwise, search the CWD using the standard order.
pub fn resolve_file(
    path: Option<&str>,
    conf: Option<&str>,
) -> Result<PathBuf, ParseError> {
    if let Some(p) = path {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Ok(pb);
        }
        return Err(ParseError::FileNotFound(pb));
    }

    if let Some(c) = conf {
        let pb = PathBuf::from(c);
        if pb.exists() {
            return Ok(pb);
        }
        return Err(ParseError::FileNotFound(pb));
    }

    for filename in FILE_SEARCH_ORDER {
        let pb = PathBuf::from(filename);
        if pb.exists() {
            return Ok(pb);
        }
    }

    Err(ParseError::NoFileFound)
}

/// Parse a task file at the given path, with optional CLI variables.
/// For .star files, vars are injected into the Starlark evaluator as __cli_vars__.
/// For .sh files, vars are ignored at parse time (they become env vars at execution time).
pub fn parse_file(path: &Path, vars: &HashMap<String, String>) -> Result<ParseResult, ParseError> {
    let content = std::fs::read_to_string(path).map_err(|e| ParseError::IoError {
        path: path.to_path_buf(),
        source: e,
    })?;

    match path.extension().and_then(|e| e.to_str()) {
        Some("star") => starlark_parser::parse_starlark(path, &content, vars),
        Some("sh") => {
            let parser = bash_parser::BashParser;
            parser.parse(path, &content)
        }
        Some(ext) => Err(ParseError::UnknownFormat(ext.to_string())),
        None => Err(ParseError::UnknownFormat("(no extension)".to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_search_order_correct() {
        assert_eq!(
            FILE_SEARCH_ORDER,
            &["Energize.star", "energize.star", "Energize.sh", "energize.sh"]
        );
    }

    #[test]
    fn resolve_file_not_found() {
        let result = resolve_file(Some("/nonexistent/path/Energize.sh"), None);
        assert!(result.is_err());
    }
}
