FROM nixos/nix:latest AS builder
WORKDIR /app

RUN nix-channel --update
RUN echo "experimental-features = nix-command flakes" >> /etc/nix/nix.conf

# Copy the entire source directory, including Cargo files and the flake.
# This ensures Nix has everything it needs to perform the build.
COPY src src
COPY Cargo.toml Cargo.toml
COPY Cargo.lock Cargo.lock
COPY flake.nix flake.nix
COPY build.rs build.rs


RUN nix build .#default -o result > /dev/null

RUN BINARY_PATH=$(readlink -f result)/bin/crypto-collector && cp "$BINARY_PATH" /crypto-collector

FROM scratch AS runtime
WORKDIR /app

USER 1000
COPY --from=builder /etc/ssl/certs/ca-bundle.crt /etc/ssl/certs/ca-bundle.crt
COPY --from=builder /crypto-collector /app/crypto-collector

ENTRYPOINT ["/app/crypto-collector"]
