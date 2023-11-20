# AWS lambda in Rust
## Requirements
- Rust toolchain: https://www.rust-lang.org/tools/install
- Cargo lambda: https://www.cargo-lambda.info/guide/getting-started.html

## Building
`cargo lambda build --output-format zip --release`

This will create a bootstrap.zip file under target\lambda folder
Upload this file to AWS lambda portal
