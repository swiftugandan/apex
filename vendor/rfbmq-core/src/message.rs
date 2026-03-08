use std::fmt::Write;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::str::FromStr;

use crate::error::{Error, Result};
use crate::types::{Header, Message, MessageId, Priority, MAX_MESSAGE_SIZE};

const HEADER_READ_LIMIT: usize = 8192;

#[deprecated(note = "use MessageId::generate() instead")]
pub fn generate_id() -> String {
    MessageId::generate().to_string()
}

impl Message {
    pub fn serialize(&self) -> Result<String> {
        // Early size check on body as a fast path to avoid allocating a huge
        // buffer only to reject it.
        if self.body.len() > MAX_MESSAGE_SIZE {
            return Err(Error::MessageTooLarge {
                size: self.body.len(),
                max: MAX_MESSAGE_SIZE,
            });
        }

        let h = &self.header;
        let id = h.id.as_ref().expect("MessageId must be set before serializing");
        let mut buf = String::with_capacity(512 + self.body.len());

        // Writing to a String via fmt::Write is infallible (only fails on OOM,
        // which aborts), so unwrap() is appropriate here.
        write!(buf, "Id: {}\n", id).unwrap();
        write!(buf, "Created-At: {}\n", h.created_at).unwrap();
        write!(buf, "Created-By: {}\n", h.created_by).unwrap();
        write!(buf, "Priority: {}\n", h.priority).unwrap();
        write!(buf, "Retry-Count: {}\n", h.retry_count).unwrap();
        write!(buf, "TTL: {}\n", h.ttl).unwrap();

        if !h.tags.is_empty() {
            write!(buf, "Tags: {}\n", h.tags.join(", ")).unwrap();
        }
        if let Some(ref cid) = h.correlation_id {
            write!(buf, "Correlation-Id: {}\n", cid).unwrap();
        }
        if let Some(ref rt) = h.reply_to {
            write!(buf, "Reply-To: {}\n", rt).unwrap();
        }
        if !h.depends_on.is_empty() {
            let dep_str: Vec<&str> = h.depends_on.iter().map(|id| id.as_str()).collect();
            write!(buf, "Depends-On: {}\n", dep_str.join(", ")).unwrap();
        }
        if !h.custom.is_empty() {
            buf.push_str("Custom:\n");
            for line in &h.custom {
                write!(buf, "  {}\n", line).unwrap();
            }
        }

        buf.push('\n');
        buf.push_str(&self.body);

        if buf.len() > MAX_MESSAGE_SIZE {
            return Err(Error::MessageTooLarge {
                size: buf.len(),
                max: MAX_MESSAGE_SIZE,
            });
        }

        Ok(buf)
    }

    pub fn from_file(path: &Path) -> Result<Message> {
        parse_file(path)
    }
}

impl FromStr for Message {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        parse_content(s)
    }
}

pub fn parse_file(path: &Path) -> Result<Message> {
    let content = fs::read_to_string(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::FileNotFound(path.to_path_buf())
        } else {
            Error::Io(e)
        }
    })?;

    if content.len() > MAX_MESSAGE_SIZE {
        return Err(Error::MessageTooLarge {
            size: content.len(),
            max: MAX_MESSAGE_SIZE,
        });
    }

    parse_content(&content)
}

pub fn parse_headers(path: &Path) -> Result<Message> {
    let mut f = fs::File::open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::FileNotFound(path.to_path_buf())
        } else {
            Error::Io(e)
        }
    })?;

    let mut buf = vec![0u8; HEADER_READ_LIMIT];
    let n = f.read(&mut buf).map_err(Error::Io)?;
    buf.truncate(n);

    let slice = String::from_utf8_lossy(&buf);
    let normalized = normalize_line_endings(&slice);

    let header_str = if let Some(pos) = normalized.find("\n\n") {
        &normalized[..pos]
    } else {
        &normalized
    };

    let header = parse_header_block(header_str)?;
    Ok(Message {
        header,
        body: String::new(),
    })
}

/// Normalize CRLF to LF for consistent parsing.
fn normalize_line_endings(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

fn parse_content(content: &str) -> Result<Message> {
    let normalized = normalize_line_endings(content);

    let (header_str, body) = if let Some(pos) = normalized.find("\n\n") {
        (&normalized[..pos], normalized[pos + 2..].to_string())
    } else {
        (normalized.as_str(), String::new())
    };

    let header = parse_header_block(header_str)?;
    Ok(Message { header, body })
}

fn parse_header_block(block: &str) -> Result<Header> {
    let mut header = Header::default();
    let mut in_custom = false;
    let mut custom_lines: Vec<String> = Vec::new();

    for line in block.lines() {
        // RFC 822 continuation line (starts with whitespace)
        if line.starts_with(' ') || line.starts_with('\t') {
            if in_custom {
                custom_lines.push(line.trim().to_string());
            }
            continue;
        }

        in_custom = false;

        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();

        match key.to_lowercase().as_str() {
            "id" => {
                let id: MessageId = value.parse().map_err(|_| {
                    Error::Parse(format!(
                        "invalid ID: '{}' (expected {} hex chars)",
                        value,
                        crate::types::ID_LEN
                    ))
                })?;
                header.id = Some(id);
            }
            "created-at" => header.created_at = value.to_string(),
            "created-by" => header.created_by = value.to_string(),
            "priority" => header.priority = Priority::from_str_or_default(value),
            "retry-count" => {
                header.retry_count = value
                    .parse()
                    .map_err(|_| Error::Parse(format!("invalid retry count: {}", value)))?;
            }
            "ttl" => {
                header.ttl = value
                    .parse()
                    .map_err(|_| Error::Parse(format!("invalid TTL: {}", value)))?;
            }
            "tags" => {
                header.tags = value
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            "correlation-id" => header.correlation_id = Some(value.to_string()),
            "reply-to" => {
                // Reply-To value may contain colons (e.g. paths), so rejoin
                let full_value = line.splitn(2, ':').nth(1).unwrap_or("").trim();
                header.reply_to = Some(full_value.to_string());
            }
            "depends-on" => {
                header.depends_on = value
                    .split(',')
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.parse::<MessageId>().map_err(|_| {
                        Error::Parse(format!("invalid dependency ID: '{}'", s))
                    }))
                    .collect::<Result<Vec<MessageId>>>()?;
            }
            "custom" => {
                in_custom = true;
            }
            _ => {}
        }
    }

    if !custom_lines.is_empty() {
        header.custom = custom_lines;
    }

    Ok(header)
}
