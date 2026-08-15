FROM rust:1.97-slim

RUN apt update && \
    apt install -y rpm libfontconfig1-dev libssl-dev && \
    cargo install cargo-bundle && \
    cargo install cargo-generate-rpm

COPY fitsmith /opt/fitz/fitsmith/
COPY fitz-cli /opt/fitz/fitz-cli/
COPY libfitz /opt/fitz/libfitz/
COPY Cargo.* /opt/fitz/
COPY build/linux/run-build.sh /opt/fitz/build/linux/

WORKDIR /opt/fitz

ENTRYPOINT ["build/linux/run-build.sh"]
