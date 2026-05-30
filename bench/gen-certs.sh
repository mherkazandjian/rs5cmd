#!/usr/bin/env bash
# Generate a self-signed cert for the `minio-tls` docker-compose service, used to
# verify the HTTPS / --no-verify-ssl paths against a local TLS S3 endpoint.
# The cert is git-ignored (never commit private keys); run this once after clone.
set -euo pipefail

dir="$(cd "$(dirname "$0")" && pwd)/certs"
mkdir -p "$dir"

openssl req -x509 -newkey rsa:2048 -nodes \
  -keyout "$dir/private.key" -out "$dir/public.crt" \
  -days 3650 -subj "/CN=minio-tls" \
  -addext "subjectAltName=DNS:minio-tls,DNS:localhost,IP:127.0.0.1"

echo "wrote $dir/private.key and $dir/public.crt"
