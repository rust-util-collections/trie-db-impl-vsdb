all: fmt lint

fmt:
	cargo +nightly fmt

lint:
	cargo clippy --workspace
	cargo clippy --workspace --features="serde_derive"
	cargo clippy --workspace --tests
	cargo clippy --workspace --tests --features="serde_derive"
	@ # cargo clippy --workspace --examples
	@ # cargo clippy --workspace --features="benchmark"

musl_lint:
	if [ `uname -s` = "Linux" ]; then \
		cargo clippy --workspace --target=x86_64-unknown-linux-musl; \
	fi

test:
	cargo test --workspace -- --nocapture
	cargo test --workspace --features="serde_derive" -- --nocapture

update:
	rustup update stable
	cargo update

clean:
	cargo clean
