use anyhow::{Context, Result};

pub fn format(prefix: &str, segment_id: u32) -> String {
    // The format string specifies a 10-digit number, so we pad with leading zeros from
    // the left. This assumes that segment_id is a 32-bit integer, which is confirmed by
    // the assert below. If you came here because it failed due to changing it to u64,
    // you will need to update the format string as well.
    assert_eq!(segment_id.to_le_bytes().len(), 4);
    format!("{prefix}.{segment_id:0>10}.log")
}

pub fn parse(prefix: &str, filename: &str) -> Result<u32> {
    // The filename of a segment file consists of a configurable prefix, a 10-digit segment ID,
    // and a ".log" suffix.
    //
    // Example: "prefix.0000000001.log".
    // Extract the segment ID from the filename
    assert!(!prefix.is_empty());
    let without_prefix = match filename.strip_prefix(prefix) {
        Some(s) => s,
        None => {
            return Err(anyhow::anyhow!(
                "Invalid segment filename format: missing prefix"
            ))
        }
    };

    let without_suffix = match without_prefix.strip_suffix(".log") {
        Some(s) => s,
        None => {
            return Err(anyhow::anyhow!(
                "Invalid segment filename format: missing .log suffix"
            ))
        }
    };

    let segment_id_str = match without_suffix.strip_prefix('.') {
        Some(s) => s,
        None => {
            return Err(anyhow::anyhow!(
                "Invalid segment filename format: missing dot separator"
            ))
        }
    };

    // Check that the segment ID string has exactly 10 digits
    if segment_id_str.len() != 10 {
        return Err(anyhow::anyhow!(
            "Invalid segment filename format: segment ID must be exactly 10 digits"
        ));
    }

    // Parse the segment ID as a u32
    let segment_id = segment_id_str
        .parse::<u32>()
        .context("Failed to parse segment ID")?;

    Ok(segment_id)
}

#[cfg(test)]
mod tests {
    use super::{format, parse};

    #[test]
    fn test_filename_isomorphism() {
        let test_cases = vec![
            ("prefix", 0),
            ("prefix", 1),
            ("prefix", 9999),
            ("prefix", u32::MAX),
            ("log", 42),
            ("segment", 1000000),
            ("very_long_prefix_name", 12345),
            ("a", 987654321),
        ];

        for (prefix, id) in test_cases {
            let filename = format(prefix, id);
            let parsed_id = parse(prefix, &filename).unwrap();
            assert_eq!(
                id, parsed_id,
                "Mismatch for prefix '{}' and id {}",
                prefix, id
            );
        }
    }

    #[test]
    fn test_parse_segment_filename_edge_cases() {
        // Valid cases
        assert_eq!(parse("prefix", "prefix.0000000000.log").unwrap(), 0);
        assert_eq!(parse("prefix", "prefix.0000000001.log").unwrap(), 1);
        assert_eq!(parse("prefix", "prefix.4294967295.log").unwrap(), u32::MAX);
        assert_eq!(parse("a", "a.0000000042.log").unwrap(), 42);

        // Invalid cases
        assert!(parse("prefix", "prefix.00000000001.log").is_err()); // Too many digits
        assert!(parse("prefix", "prefix.000000001.log").is_err()); // Too few digits
        assert!(parse("prefix", "prefix.000000000a.log").is_err()); // Non-numeric ID
        assert!(parse("prefix", "prefix.0000000000").is_err()); // Missing .log suffix
        assert!(parse("prefix", "prefix0000000000.log").is_err()); // Missing dot after prefix
        assert!(parse("prefix", "wrongprefix.0000000000.log").is_err()); // Wrong prefix
        assert!(parse("prefix", ".0000000000.log").is_err()); // Missing prefix
        assert!(parse("prefix", "prefix..log").is_err()); // Missing ID
        assert!(parse("prefix", "prefix.0000000000.wrongsuffix").is_err()); // Wrong suffix

        // Adversarial cases
        assert!(parse("prefix", "prefix.0000000000.logx").is_err()); // Extra character after .log
        assert!(parse("prefix", "xprefix.0000000000.log").is_err()); // Extra character before prefix
        assert!(parse("prefix", "prefix.00000000001log").is_err()); // Missing dot before log
        assert!(parse("prefix", "prefix.0000000000.log.").is_err()); // Extra dot at the end
        assert!(parse("prefix", "prefix.4294967296.log").is_err()); // ID overflow (u32::MAX + 1)
        assert!(parse("prefix", "prefix.0x0000000A.log").is_err()); // Hexadecimal ID
        assert_eq!(
            parse("prefix.with.dots", "prefix.with.dots.0000000000.log").unwrap(),
            0
        ); // Prefix with dots
    }
}
