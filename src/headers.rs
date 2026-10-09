//! HTTP field validation, separate from HPACK's compression state.

use crate::Error;

pub(crate) fn token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

pub(crate) fn scheme_valid(scheme: &str) -> bool {
    // RFC 3986 section 3.1: ALPHA *( ALPHA / DIGIT / "+" / "-" / "." ).
    let mut bytes = scheme.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || b"+-.".contains(&byte))
}

fn uri_name_valid(value: &str, userinfo: bool) -> bool {
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            if !bytes.next().is_some_and(|byte| byte.is_ascii_hexdigit())
                || !bytes.next().is_some_and(|byte| byte.is_ascii_hexdigit())
            {
                return false;
            }
        } else if !byte.is_ascii_alphanumeric()
            && !b"-._~!$&'()*+,;=".contains(&byte)
            && !(userinfo && byte == b':')
        {
            return false;
        }
    }
    true
}

pub(crate) fn authority_valid(authority: &str, scheme: &str, connect: bool) -> bool {
    // RFC 3986 section 3.2: [userinfo@] host [:port]. HTTP(S) and CONNECT
    // prohibit userinfo; other schemes can carry it (RFC 9113 section 8.3.1).
    let http = scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https");
    let host_port = if let Some((userinfo, host_port)) = authority.split_once('@') {
        if connect || http || !uri_name_valid(userinfo, true) {
            return false;
        }
        host_port
    } else {
        authority
    };
    let (host, port) = if let Some(literal) = host_port.strip_prefix('[') {
        let Some((address, tail)) = literal.split_once(']') else {
            return false;
        };
        let valid = if let Some(versioned) = address.strip_prefix(['v', 'V']) {
            versioned.split_once('.').is_some_and(|(version, name)| {
                !version.is_empty()
                    && version.bytes().all(|byte| byte.is_ascii_hexdigit())
                    && !name.is_empty()
                    && name.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:".contains(&byte)
                    })
            })
        } else {
            address.parse::<core::net::Ipv6Addr>().is_ok()
        };
        if !valid || (!tail.is_empty() && !tail.starts_with(':')) {
            return false;
        }
        (address, tail.strip_prefix(':'))
    } else {
        let (host, port) = host_port
            .split_once(':')
            .map_or((host_port, None), |(host, port)| (host, Some(port)));
        if !uri_name_valid(host, false) {
            return false;
        }
        (host, port)
    };
    if (http || connect) && host.is_empty() {
        return false;
    }
    if connect {
        // CONNECT has no scheme default: its TCP destination requires a port.
        port.is_some_and(|port| {
            port.bytes().all(|byte| byte.is_ascii_digit())
                && port.parse::<u16>().is_ok_and(|number| number != 0)
        })
    } else {
        port.is_none_or(|port| port.bytes().all(|byte| byte.is_ascii_digit()))
    }
}

pub(crate) fn path_valid(method: &str, path: &str) -> bool {
    if path == "*" {
        return method == "OPTIONS";
    }
    // HTTP/2 uses absolute-path plus optional query, not a URI fragment.
    if !path.starts_with('/') {
        return false;
    }
    let mut bytes = path.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            if !bytes.next().is_some_and(|byte| byte.is_ascii_hexdigit())
                || !bytes.next().is_some_and(|byte| byte.is_ascii_hexdigit())
            {
                return false;
            }
        } else if !byte.is_ascii_alphanumeric() && !b"-._~!$&'()*+,;=:@/?".contains(&byte) {
            return false;
        }
    }
    true
}

pub(crate) fn value_valid(value: &str) -> bool {
    !value
        .bytes()
        .any(|byte| (byte < 0x20 && byte != b'\t') || byte == 0x7f)
        && !value.starts_with([' ', '\t'])
        && !value.ends_with([' ', '\t'])
}

pub(crate) fn field(name: &str, value: &str, request: bool) -> Result<(), Error> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| token(byte) && !byte.is_ascii_uppercase())
        || !value_valid(value)
        || matches!(
            name,
            "connection" | "proxy-connection" | "keep-alive" | "transfer-encoding" | "upgrade"
        )
        || (name == "te" && (!request || !value.eq_ignore_ascii_case("trailers")))
    {
        return Err(Error::Protocol);
    }
    Ok(())
}

// Identical duplicate/list values can be normalized (RFC 9110 section 8.6).
pub(crate) fn content_length(value: &str, length: &mut Option<u64>) -> Result<(), Error> {
    for part in value.split(',') {
        let part = part.trim_matches([' ', '\t']);
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(Error::Protocol);
        }
        let parsed = part
            .bytes()
            .try_fold(0u64, |number, byte| {
                number.checked_mul(10)?.checked_add(u64::from(byte - b'0'))
            })
            .ok_or(Error::Protocol)?;
        if length.is_some_and(|previous| previous != parsed) {
            return Err(Error::Protocol);
        }
        *length = Some(parsed);
    }
    Ok(())
}

#[derive(Default)]
pub(crate) struct ResponseHeaders {
    pub status: Option<u16>,
    pub content_length: Option<u64>,
    regular: bool,
}

impl ResponseHeaders {
    pub fn field(&mut self, name: &str, value: &str) -> Result<(), Error> {
        if name.starts_with(':') {
            if name != ":status"
                || self.regular
                || self.status.is_some()
                || value.len() != 3
                || !value.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(Error::Protocol);
            }
            let bytes = value.as_bytes();
            let status = u16::from(bytes[0] - b'0') * 100
                + u16::from(bytes[1] - b'0') * 10
                + u16::from(bytes[2] - b'0');
            if !(100..=599).contains(&status) || status == 101 {
                return Err(Error::Protocol);
            }
            self.status = Some(status);
        } else {
            field(name, value, false)?;
            self.regular = true;
            if name == "content-length" {
                content_length(value, &mut self.content_length)?;
            }
        }
        Ok(())
    }
}
