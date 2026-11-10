# bohr in a container: the static release binary and its target packs on a small Debian.
#
# Built from a release's own files, never from source, so the image runs exactly the binary the
# release signed: put `bohr`, `bohr-packs-<version>.tar.gz` and `scripts/docker-help.txt` beside
# this file, then
#
#   docker build --build-arg VERSION=v1.0.0 -t ghcr.io/kdaniello/bohr:v1.0.0 .
#
# A base with a shell and a C library rather than none: the programs bohr runs are the user's,
# and most of them are linked against glibc and started by a dynamic loader.
FROM debian:12.8-slim

ARG VERSION
LABEL org.opencontainers.image.title="bohr" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.source="https://github.com/KDaniello/bohr" \
      org.opencontainers.image.description="Deterministic simulation testing. Needs: docker run --cap-add SYS_PTRACE --security-opt seccomp=unconfined --security-opt apparmor=unconfined" \
      org.opencontainers.image.licenses="Apache-2.0"

COPY bohr /usr/local/bin/bohr
COPY bohr-packs-${VERSION}.tar.gz /tmp/packs.tar.gz
RUN mkdir -p /root/.bohr/packs \
 && tar -xzf /tmp/packs.tar.gz -C /root/.bohr/packs \
 && rm /tmp/packs.tar.gz \
 && chmod 755 /usr/local/bin/bohr

COPY docker-help.txt /usr/share/doc/bohr/docker.txt
WORKDIR /work
ENTRYPOINT ["/usr/local/bin/bohr"]
# Run with no arguments, the image checks what its container lets it do and says which flag is
# missing: ptrace, the namespaces a guest is isolated in, and the seccomp profile that hides both.
CMD ["doctor"]
