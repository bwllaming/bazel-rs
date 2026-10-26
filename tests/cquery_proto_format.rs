//! No Bazel, Python, network, or unfinished CLI is needed for these tests. The
//! bytes and owned DTO source were captured/decoded using upstream Bazel schemas.

use bazel_rs::cquery_proto::*;

#[path = "fixtures/cquery_proto/captured.rs"]
mod captured;

fn replay(format: OutputFormat, include_configurations: bool) -> Vec<u8> {
    let (targets, configs) = captured::captured();
    let mut writer = CqueryWriter::new(
        Vec::new(),
        format,
        WriterOptions {
            transitions: Transitions::Lite,
            include_configurations,
            include_attribute_source_aspects: true,
            ..WriterOptions::default()
        },
    );
    for (target, id) in &targets {
        writer
            .write_target(target, configs.iter().find(|config| config.id == *id))
            .unwrap();
    }
    writer.write_configurations(&configs).unwrap();
    writer.finish().unwrap()
}

#[test]
fn proto_matches_genuine_bazel_93_capture_byte_for_byte() {
    let bytes = replay(OutputFormat::Proto, true);
    assert_eq!(bytes, include_bytes!("fixtures/cquery_proto/genuine.proto"));
    // Optional artifact for the independent Python/upstream-schema check, not a
    // cache and never needed for the test's verdict.
    if let Some(dir) = std::env::var_os("BAZEL_RS_CQUERY_PROTO_DUMP") {
        std::fs::write(std::path::Path::new(&dir).join("native.proto"), bytes).unwrap();
    }
}

#[test]
fn streamed_proto_matches_independently_framed_genuine_capture() {
    let bytes = replay(OutputFormat::StreamedProto, true);
    assert_eq!(
        bytes,
        include_bytes!("fixtures/cquery_proto/canonical.streamed_proto")
    );
    if let Some(dir) = std::env::var_os("BAZEL_RS_CQUERY_PROTO_DUMP") {
        std::fs::write(
            std::path::Path::new(&dir).join("native.streamed_proto"),
            bytes,
        )
        .unwrap();
    }
}

#[test]
fn no_configurations_uses_query_result_in_both_formats() {
    assert_eq!(
        replay(OutputFormat::Proto, false),
        include_bytes!("fixtures/cquery_proto/canonical.no_config.proto")
    );
    assert_eq!(
        replay(OutputFormat::StreamedProto, false),
        include_bytes!("fixtures/cquery_proto/canonical.no_config.streamed_proto")
    );
}

#[test]
fn captured_owned_dtos_roundtrip_through_postcard() {
    let dto = captured::captured();
    let bytes = postcard::to_stdvec(&dto).unwrap();
    let restored: (Vec<(Target, u32)>, Vec<Configuration>) = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(restored, dto);
    assert!(bytes.len() < include_bytes!("fixtures/cquery_proto/genuine.proto").len());
}

#[test]
fn corpus_has_every_supported_nonlegacy_attribute_type_and_presence() {
    let (targets, _) = captured::captured();
    let mut kinds = std::collections::BTreeSet::new();
    let mut aspect = false;
    let mut unset = false;
    for (target, _) in targets {
        if let Target::Rule(rule) = target {
            for attr in rule.attributes {
                kinds.insert(attr.value.attribute_type() as u8);
                aspect |= attr.source_aspect_name.is_some_and(|s| !s.is_empty());
                unset |= matches!(attr.value, AttributeValue::Unset(_));
            }
        }
    }
    // OUTPUT_LIST/LICENSE are exercised by genrule. DistributionSet/Unknown
    // are legacy schema types not exposed by this fixture's Starlark attr API.
    for kind in [1, 2, 3, 4, 5, 6, 7, 9, 10, 12, 13, 14, 15, 16, 19, 21] {
        assert!(
            kinds.contains(&kind),
            "missing Attribute discriminator {kind}"
        );
    }
    assert!(aspect && unset);
}
