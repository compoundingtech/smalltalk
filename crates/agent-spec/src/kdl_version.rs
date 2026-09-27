use anyhow::{Context as _, Result, bail, ensure};
use kdl::{KdlDocument, KdlValue};

pub const ST2_KDL_VERSIONS: [u32; 2] = [0, 1];
pub const ST3_KDL_VERSION: u32 = 2;

/// Return the declared document version. A missing declaration is version zero.
pub fn document_version(document: &KdlDocument) -> Result<u32> {
    let mut declarations = document
        .nodes()
        .iter()
        .filter(|node| node.name().value() == "version");
    let Some(node) = declarations.next() else {
        return Ok(0);
    };
    ensure!(
        declarations.next().is_none(),
        "a KDL document can contain only one version declaration"
    );
    ensure!(node.ty().is_none(), "the KDL version cannot have a type");
    ensure!(
        node.children().is_none(),
        "the KDL version cannot have children"
    );
    ensure!(
        node.entries().len() == 1 && node.entries()[0].name().is_none(),
        "the KDL version must contain one integer"
    );
    let KdlValue::Integer(value) = node.entries()[0].value() else {
        bail!("the KDL version must contain one integer");
    };
    u32::try_from(*value).context("the KDL version is outside the supported integer range")
}

pub fn ensure_st2_version(document: &KdlDocument) -> Result<u32> {
    let version = document_version(document)?;
    ensure!(
        ST2_KDL_VERSIONS.contains(&version),
        "st2 accepts KDL version 0 or 1, but this document uses version {version}"
    );
    Ok(version)
}

pub fn ensure_st3_version(document: &KdlDocument) -> Result<u32> {
    let version = document_version(document)?;
    ensure!(
        version == ST3_KDL_VERSION,
        "st3 requires KDL version 2, but this document uses version {version}"
    );
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_version_is_st2_version_zero() {
        let document: KdlDocument = "agent \"worker\"".parse().unwrap();
        assert_eq!(document_version(&document).unwrap(), 0);
        assert_eq!(ensure_st2_version(&document).unwrap(), 0);
        assert!(ensure_st3_version(&document).is_err());
    }

    #[test]
    fn each_runtime_accepts_only_its_versions() {
        for version in [0, 1] {
            let document: KdlDocument = format!("version {version}\nagent \"worker\"")
                .parse()
                .unwrap();
            assert_eq!(ensure_st2_version(&document).unwrap(), version);
            assert!(ensure_st3_version(&document).is_err());
        }
        let document: KdlDocument =
            "version 2\nmission \"work\" state=\"ready\" { goal \"Do the work.\" }"
                .parse()
                .unwrap();
        assert_eq!(ensure_st3_version(&document).unwrap(), 2);
        assert!(ensure_st2_version(&document).is_err());
    }

    #[test]
    fn malformed_or_repeated_versions_fail() {
        for source in [
            "version \"two\"",
            "version version=2",
            "version 2 { child }",
            "version 1\nversion 2",
        ] {
            let document: KdlDocument = source.parse().unwrap();
            assert!(document_version(&document).is_err(), "{source}");
        }
    }
}
