// docs/rpc.md is the reference for every method the node answers. A method added
// to METHODS without a section there, or a section left behind for a method that
// is gone, fails here rather than in a user's hands.

use plaine_rpc::methods::METHODS;

fn reference() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/rpc.md");
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

/// The method names that have a `## \`name\`` section, in document order.
fn sections(doc: &str) -> Vec<&str> {
    doc.lines()
        .filter_map(|l| l.strip_prefix("## `"))
        .filter_map(|rest| rest.split('`').next())
        .collect()
}

#[test]
fn every_method_has_a_section_and_every_section_a_method() {
    let doc = reference();
    let documented = sections(&doc);
    for m in METHODS {
        assert!(documented.contains(m), "{m} is answered by the node but has no section in docs/rpc.md");
    }
    for d in &documented {
        assert!(METHODS.contains(d), "docs/rpc.md documents {d}, which the node does not answer");
    }
    assert_eq!(documented.len(), METHODS.len(), "a method has two sections in docs/rpc.md");
}

#[test]
fn sections_follow_the_method_list() {
    let doc = reference();
    assert_eq!(sections(&doc), METHODS, "docs/rpc.md lists the methods in METHODS order");
}

#[test]
fn every_section_has_the_four_parts() {
    let doc = reference();
    for chunk in doc.split("\n## `").skip(1) {
        let name = chunk.split('`').next().unwrap_or_default();
        let body = chunk.split("\n---").next().unwrap_or_default();
        for part in ["### Parameters", "### Result", "### Errors", "### Example"] {
            assert!(body.contains(part), "{name} in docs/rpc.md has no {part:?}");
        }
    }
}

#[test]
fn the_summary_list_names_every_method() {
    let doc = reference();
    let summary = doc
        .split("### Methods")
        .nth(1)
        .and_then(|s| s.split("\n---").next())
        .expect("docs/rpc.md has a Methods summary");
    for m in METHODS {
        assert!(summary.contains(&format!("`{m}`")), "{m} is missing from the Methods summary");
    }
}

#[test]
fn every_example_sends_json() {
    // curl -d without a Content-Type sends a form, which the node answers with 415.
    let doc = reference();
    for (i, line) in doc.lines().enumerate() {
        if line.starts_with("curl ") {
            assert!(
                line.contains("-H 'Content-Type: application/json'"),
                "docs/rpc.md:{}: a curl example without the JSON content type gets HTTP 415",
                i + 1
            );
        }
    }
}
