# A self-contained Neon toolchain image, on glibc.
#
# Neon compiles to C and shells out to `cc` at run time, so the image carries a C compiler
# as well as the toolchain. Everything is built and run on the same glibc: the neon binary,
# the prebuilt runtime archives, and the `cc` that later links a user's program all share
# one libc, which is what keeps a program compiled inside the image actually link.
#
# Debian slim rather than Alpine on purpose — glibc, not musl. The image is larger for it,
# but it avoids musl's allocator and resolver differences, and matches the libc almost every
# host and the prebuilt archives expect.
#
# The point is portability: `docker run` (or Podman, or Docker Desktop on Windows) gives a
# working `neon` where installing a Rust + C toolchain directly is awkward or disallowed.

# ---- builder ----
FROM rust:slim-bookworm AS build

# The rust image already carries gcc + libc6-dev for building C-dependency crates; cmake is
# what it lacks, and the runtime is a CMake project driven by neon-runtime's build script.
RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake make \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .
RUN cargo build --release --locked

# `cargo build` stages the sysroot (include/, lib/<flavor>/, stdlib/) next to the binaries
# in target/release. Rearrange into an install prefix: bin/ beside lib/, include/, stdlib/,
# which is the layout `Sysroot::find` resolves from the binary's parent directory.
RUN set -eux; \
    mkdir -p /out/bin; \
    cp target/release/neon /out/bin/; \
    cp -r target/release/include target/release/lib target/release/stdlib /out/

# ---- final ----
FROM debian:bookworm-slim

# gcc + libc6-dev: `neon build` invokes the C compiler and links the C standard
# headers/libs. CC=gcc so neon names the compiler directly rather than relying on the `cc`
# alternative being wired up.
RUN apt-get update \
    && apt-get install -y --no-install-recommends gcc libc6-dev \
    && rm -rf /var/lib/apt/lists/*
ENV CC=gcc

# Into /usr/local, so /usr/local/bin/neon finds /usr/local/{lib,include,stdlib} one level up.
COPY --from=build /out/ /usr/local/

# Fail the build if the toolchain is not actually wired up in the image, rather than shipping
# one that only breaks when a user runs it.
RUN neon doctor

WORKDIR /work
ENTRYPOINT ["neon"]
CMD ["--help"]
