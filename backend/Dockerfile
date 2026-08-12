# Build image for `decky plugin build`'s custom-backend step.
#
# The decky CLI builds this Dockerfile, runs the resulting image with this
# directory mounted, and copies whatever lands in `out/` into the plugin zip's
# `bin/`. Two binaries come out, and they are for *different operating
# systems*:
#
#   ct2trainer               x86-64 Linux ELF — runs on the Deck
#   deckytrainers-x86_64.exe x86-64 Windows PE — embedded into every trainer,
#                                                runs under the game's Proton
#                                                prefix
#
# That asymmetry is the whole design. A trainer has to be a Windows process to
# use ReadProcessMemory/VirtualAllocEx/CreateRemoteThread against a Proton
# game — the generator has to be
# a Linux process because Decky runs it. See CLAUDE.md.
#
# `--platform=linux/amd64` is load-bearing, not boilerplate. Docker builds for
# the host architecture by default, so on an Apple Silicon machine this produces
# aarch64 ELFs that the (x86-64) Deck cannot execute at all — and nothing checks
# the architecture between here and the device. Pinning it means the same
# artifacts come out of any dev machine, at the cost of an emulated build on
# arm64 hosts.
#
# Debian bookworm on purpose: its glibc (2.36) is older than SteamOS 3.x's, so
# a binary built here runs on the Deck. Building against a *newer* glibc than
# the target has is the classic way to produce something that only fails on the
# device. If a crate ever needs a newer one, switch the entrypoint to
# `--target x86_64-unknown-linux-musl` instead of moving to a newer base.
# (This constrains ct2trainer only — the trainer runtime links against Wine's
# libraries, not this image's glibc.)
FROM --platform=linux/amd64 rust:1-bookworm

# `decky plugin build` runs the container as the host user (`--user uid:gid`),
# not root, so every path cargo writes has to be writable by an arbitrary uid.
# The image's own CARGO_HOME is root-owned; give it one of its own.
ENV CARGO_HOME=/cargo
RUN mkdir -p /cargo && chmod -R 0777 /cargo

# The Windows cross toolchain for the trainer runtime.
#
# `-gnu` rather than `-msvc`: mingw-w64 is one apt package, whereas the MSVC
# target needs Microsoft's headers and import libraries fetched separately.
# The runtime cross-compiles cleanly here, including mlua's vendored Lua 5.4 C
# sources, which is the part that actually needed a C compiler.
RUN apt-get update \
 && apt-get install -y --no-install-recommends gcc-mingw-w64-x86-64 \
 && rm -rf /var/lib/apt/lists/*
RUN rustup target add x86_64-pc-windows-gnu \
 && chmod -R 0777 /usr/local/rustup

WORKDIR /backend

# The CLI's mount point isn't part of its documented contract, so find the
# source rather than assume it: whichever candidate carries entrypoint.sh is
# the checkout being built.
ENTRYPOINT ["/bin/bash", "-c", "for d in /backend /plugin/backend \"$PWD\"; do if [ -f \"$d/entrypoint.sh\" ]; then exec /bin/bash \"$d/entrypoint.sh\"; fi; done; echo 'entrypoint.sh not found in /backend, /plugin/backend or $PWD' >&2; exit 1"]
