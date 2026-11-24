# Dependency license inputs

Rust crate archives normally supply their own license files. The notice
generator uses these files when an archive omits its license text.

- [Ruff 0.0.12](ruff-0.0.12.txt) copies the upstream MIT license and copyright
  notice at the revision recorded in the Ruff crate archives.
  [Pinned Ruff license](https://github.com/astral-sh/ruff/blob/22f65a2ab5052990503985c7c794de37598d531e/LICENSE)
- [MIT terms](mit-terms.txt) accompany the MIT declaration for `quote-use` and
  `quote-use-macros` 0.8.4. Their crate manifests identify the ModProg project,
  and their pinned source omits a separate copyright notice and license file.
  [Pinned quote-use manifest](https://github.com/ModProg/quote-use/blob/05096f346f8b17fba8a57a235b958917ad9d99e0/Cargo.toml)
- The `r-efi` 6.0.0 crate supplies its copyright and license in the
  registry archive's `AUTHORS` file. The generator reads it directly from
  the locked crate.

- [Oxc 0.159.0](oxc-0.159.0.txt) copies the upstream MIT license and
  copyright notices at the revision recorded in the Oxc crate archives.
  [Pinned Oxc license](https://github.com/oxc-project/oxc/blob/d9483516f84068f867e0a3949ac0ec53ef82aa63/LICENSE)

- The [C++](tree-sitter-cpp-0.23.4.txt) and [Java](tree-sitter-java-0.23.5.txt)
  grammar crate archives omit the MIT license files. The notice generator uses
  the upstream texts at the source revisions in the archives.
  [Pinned C++ license](https://github.com/tree-sitter/tree-sitter-cpp/blob/f41e1a044c8a84ea9fa8577fdd2eab92ec96de02/LICENSE) ·
  [Pinned Java license](https://github.com/tree-sitter/tree-sitter-java/blob/94703d5a6bed02b98e438d7cad1136c01a60ba2c/LICENSE)

- The Go reachability regression uses the generated module and source inputs
  from the pinned Skylos subtest. The CLI test sources ship a copy of the
  [Skylos license](../crates/cli/tests/fixtures/skylos-5.7.0/LICENSE).
  [Pinned fixture](https://github.com/duriantaco/skylos/blob/87978e69adf91b3811e97b00c40aa39d7937cd94/skylos/engines/go/internal/symbols/reachability_test.go#L13-L20)
