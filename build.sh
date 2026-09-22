#!/usr/bin/env sh
# Сборка в контейнере: на сервере нет линковщика (cc) и прав на apt.
# Результат — статический musl-бинарник, которому не нужна системная glibc.
#
#   ./build.sh            # отладочная сборка + тесты
#   ./build.sh release    # релизный бинарник в target/musl/x86_64-unknown-linux-musl/release/ai-dev
set -eu
IMAGE="${RUST_IMAGE:-rust:1-alpine}"
ROOT="$(cd "$(dirname "$0")" && pwd)"
mkdir -p "$ROOT/target/musl" "${CARGO_HOME:-$HOME/.cargo}/registry"
run() {
  docker run --rm \
    -u "$(id -u):$(id -g)" \
    -e CARGO_HOME=/cargo \
    -v "$ROOT:/work" \
    -v "${CARGO_HOME:-$HOME/.cargo}:/cargo" \
    -w /work \
    "$IMAGE" "$@"
}
case "${1:-debug}" in
  release) run cargo build --release --target-dir target/musl ;;
  test)    run cargo test --target-dir target/musl ;;
  clippy)
    # clippy в образе нет: ставим компонент внутри контейнера (нужен root).
    # Свой CARGO_HOME — иначе шим cargo-clippy ищется в смонтированном
    # хозяйском ~/.cargo, где его нет.
    mkdir -p "$ROOT/target/clippy-cargo"
    docker run --rm \
      -e CARGO_HOME=/work/target/clippy-cargo \
      -v "$ROOT:/work" \
      -w /work \
      "$IMAGE" sh -c "rustup component add clippy >/dev/null 2>&1 || true; \
        cargo clippy --all-targets --target-dir target/clippy -- -D warnings; \
        rc=\$?; chown -R $(id -u):$(id -g) target 2>/dev/null || true; exit \$rc"
    ;;
  fmt)
    # rustfmt, как и clippy, в образе отсутствует — ставим внутрь контейнера.
    mkdir -p "$ROOT/target/clippy-cargo"
    docker run --rm \
      -e CARGO_HOME=/work/target/clippy-cargo \
      -v "$ROOT:/work" \
      -w /work \
      "$IMAGE" sh -c "rustup component add rustfmt >/dev/null 2>&1 || true; \
        cargo fmt ${FMT_ARGS:---check}; \
        rc=\$?; chown -R $(id -u):$(id -g) target src tests 2>/dev/null || true; exit \$rc"
    ;;
  *)       run cargo build --target-dir target/musl ;;
esac
