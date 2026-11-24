use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn scan(path: &Path, args: &[&str], code: i32) -> Value {
    let scan = Command::new(env!("CARGO_BIN_EXE_skarmory"))
        .env_remove("SKYLOS_CONFIG_FILE")
        .env_remove("DJANGO_SETTINGS_MODULE")
        .args([
            "--profile",
            "skylos-0.3",
            "--confidence",
            "0",
            "--no-grep-verify",
        ])
        .args(args)
        .arg(path)
        .output()
        .unwrap();
    assert_eq!(
        scan.status.code(),
        Some(code),
        "{}\n{}",
        String::from_utf8_lossy(&scan.stderr),
        String::from_utf8_lossy(&scan.stdout)
    );
    serde_json::from_slice(&scan.stdout).unwrap()
}

fn unused(report: &Value) -> Vec<(&str, &str)> {
    report["unused_functions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|finding| {
            assert_eq!(finding["rule_id"], "SKY-U001");
            (
                finding["file"].as_str().unwrap(),
                finding["simple_name"].as_str().unwrap(),
            )
        })
        .collect()
}

#[test]
fn discovers_documented_cpp_source_and_header_suffixes() {
    let tree = tempfile::tempdir().unwrap();
    for extension in ["cpp", "cc", "cxx", "hpp", "hh", "hxx", "c", "h"] {
        fs::write(
            tree.path().join(format!("source.{extension}")),
            "int exported() { return 0; }\n",
        )
        .unwrap();
    }
    let report = scan(tree.path(), &[], 0);
    assert_eq!(report["analysis_summary"]["languages"]["C++"], 6);
    assert_eq!(report["coverage"]["selected_files"], 6);
    assert!(unused(&report).is_empty());

    let standalone = tree.path().join("source.cxx");
    fs::write(&standalone, "static int internal() { return 0; }\n").unwrap();
    let report = scan(&standalone, &[], 0);
    assert_eq!(report["coverage"]["selected_files"], 1);
    assert_eq!(unused(&report), [("source.cxx", "internal")]);
}

#[test]
fn cpp_discovery_respects_source_and_header_extensions_in_uppercase() {
    for (source, header) in [("source.cpp", "helper.hxx"), ("source.CPP", "helper.HXX")] {
        let tree = tempfile::tempdir().unwrap();
        fs::write(
            tree.path().join(source),
            "static int unreferenced() { return 7; }\n",
        )
        .unwrap();
        fs::write(
            tree.path().join(header),
            "static int header_function() { return 4; }\n",
        )
        .unwrap();
        let report = scan(tree.path(), &[], 0);
        assert_eq!(report["analysis_summary"]["languages"]["C++"], 2);
        assert_eq!(report["coverage"]["extracted_files"], 2);
        assert_eq!(unused(&report), [(source, "unreferenced")]);
    }
}

#[test]
fn keeps_cpp_file_local_identity_after_an_edit_and_cache_reuse() {
    let tree = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let cache_path = cache.path().to_str().unwrap();
    let first = tree.path().join("one.cpp");
    let second = tree.path().join("two.cc");
    fs::write(
        first,
        "static int lookup() { return 8; }\nint main() { return lookup(); }\n",
    )
    .unwrap();
    fs::write(&second, "static int lookup() { return 9; }\n").unwrap();
    fs::write(
        tree.path().join("helpers.hxx"),
        "static int header_helper() { return 2; }\n",
    )
    .unwrap();

    let parity = || {
        let fresh = scan(tree.path(), &[], 0);
        let mut cached = scan(
            tree.path(),
            &["--workers", "4", "--cache-dir", cache_path],
            0,
        );
        let stats = cached.as_object_mut().unwrap().remove("cache").unwrap();
        assert_eq!(fresh, cached);
        (fresh, stats)
    };
    let (initial, _) = parity();
    assert_eq!(unused(&initial), [("two.cc", "lookup")]);
    assert_eq!(parity().1["hits"], 3);

    fs::write(
        second,
        "static int lookup() { return 9; }\nint consume() { return lookup(); }\n",
    )
    .unwrap();
    let (edited, stats) = parity();
    assert_eq!(stats["hits"], 2);
    assert!(unused(&edited).is_empty());
}

#[test]
fn retains_calls_addresses_and_ordinary_lexical_references() {
    let tree = tempfile::tempdir().unwrap();
    fs::write(
        tree.path().join("main.cpp"),
        r#"
static int leaf(int value) { return value * 2; }
static int called() { return leaf(4); }
static int addressable() { return 7; }
static int shadowed() { return 9; }
static int mentioned_only_in_text() { return 8; }
namespace { int unused_hidden() { return 10; } }
const char *description = "mentioned_only_in_text()";
int main() {
    auto callback = &addressable;
    int shadowed = 3;
    return callback() + called() + shadowed;
}
"#,
    )
    .unwrap();
    let report = scan(tree.path(), &[], 0);
    assert_eq!(
        unused(&report),
        [
            ("main.cpp", "shadowed"),
            ("main.cpp", "mentioned_only_in_text"),
            ("main.cpp", "unused_hidden"),
        ]
    );
    let symbols = report["dead_code_evidence"]["symbols"].as_array().unwrap();
    for name in ["leaf", "called", "addressable"] {
        assert!(symbols.iter().any(|symbol| {
            symbol["qualified_name"] == name
                && symbol["evidence"][0]["kind"] == "cpp_file_local_reference"
        }));
    }
}

#[test]
fn bounds_cpp_shadowing_to_its_declaration_and_lexical_scope() {
    let tree = tempfile::tempdir().unwrap();
    let file = tree.path().join("main.cpp");
    for consumer in [
        "int main() { int value = candidate(); int candidate = 3; return candidate + value; }",
        "int main() { { int candidate = 3; } return candidate(); }",
        "int main() { int candidate = 3; return ::candidate(); }",
        "int consume(int (*callback)(int candidate)) { return candidate(); }",
        "int main() { int callback(int candidate); return candidate(); }",
        "int main() { try {} catch (int candidate) {} return candidate(); }",
        "int main() { bool check = requires(int candidate) { candidate + 1; }; return candidate(); }",
    ] {
        fs::write(
            &file,
            format!("static int candidate() {{ return 9; }}\nstatic int orphan() {{ return 2; }}\n{consumer}"),
        )
        .unwrap();
        assert_eq!(
            unused(&scan(tree.path(), &[], 0)),
            [("main.cpp", "orphan")],
            "{consumer}"
        );
    }
    fs::write(
        &file,
        "static int candidate() { return 1; }\nint consume(int candidate) { return candidate; }",
    )
    .unwrap();
    assert_eq!(
        unused(&scan(tree.path(), &[], 0)),
        [("main.cpp", "candidate")]
    );
}

#[test]
fn retains_ambiguous_cpp_definitions() {
    let tree = tempfile::tempdir().unwrap();
    fs::write(
        tree.path().join("main.cpp"),
        r#"
template <class T> static T generic(T value) { return value; }
static int overloaded() { return 5; }
static int overloaded(int input) { return input; }
#define CALL_THROUGH(name) macro_target()
static int macro_target() { return 1; }
class Service { static int member() { return 4; } };
int exported() { return 7; }
static int unrelated() { return 8; }
"#,
    )
    .unwrap();
    assert_eq!(
        unused(&scan(tree.path(), &[], 0)),
        [("main.cpp", "unrelated")]
    );
    fs::write(
        tree.path().join("main.cpp"),
        "#define PASTE(left, right) left ## right\nstatic int maybe_named() { return 1; }",
    )
    .unwrap();
    assert!(unused(&scan(tree.path(), &[], 0)).is_empty());
}

#[test]
fn cpp_errors_and_opaque_inputs_remain_incomplete() {
    let tree = tempfile::tempdir().unwrap();
    let file = tree.path().join("main.cpp");
    for (source, kind) in [
        ("static int unfinished() {\n", "cpp_parse_error"),
        (
            "#include \"support.hpp\"\nstatic int helper() { return 1; }\n",
            "unsupported_cpp_context",
        ),
        (
            "int main() { asm(\"call hidden\"); }\nstatic int hidden() { return 2; }\n",
            "unsupported_cpp_context",
        ),
    ] {
        fs::write(&file, source).unwrap();
        let report = scan(&file, &[], 2);
        assert_eq!(report["coverage"]["status"], "incomplete");
        assert_eq!(report["analysis_errors"][0]["kind"], kind);
        assert!(unused(&report).is_empty());
    }
}

#[test]
fn attributes_can_retain_cpp_file_local_functions() {
    let tree = tempfile::tempdir().unwrap();
    fs::write(
        tree.path().join("main.cpp"),
        r#"static int started;
[[gnu::constructor]] static void initialize() { started = 1; }
static void orphan() {}
int main() { return started == 1 ? 0 : 1; }
"#,
    )
    .unwrap();
    assert_eq!(unused(&scan(tree.path(), &[], 0)), [("main.cpp", "orphan")]);
}
