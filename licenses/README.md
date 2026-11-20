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
