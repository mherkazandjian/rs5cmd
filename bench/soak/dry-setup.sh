#!/usr/bin/env bash
# Dry run: prepare the MinIO sidecar the way the real endpoints are prepared by hand.
#   - the soak bucket, a bucket outside the soak user's policy (S8) and a public scratch bucket
#     with one object (S12)
#   - a user limited by iam/soak-iam-policy.json (BUCKET replaced), as on AWS
# Runs `mc` inside the sidecar with the sidecar's root account. Safe to repeat.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
dc="$here/dc"
env_get() { sed -n "s/^$1=//p" "$here/targets/dry.env"; }
bucket="$(env_get SOAK_BUCKET)"; user="$(env_get AWS_ACCESS_KEY_ID)"; pass="$(env_get AWS_SECRET_ACCESS_KEY)"
outside="$(env_get SOAK_DENIED_BUCKET)"; public="$(env_get SOAK_PUBLIC_URL)"; public="${public#s3://}"
mc() { "$dc" exec -T minio mc "$@"; }
mc alias set local http://127.0.0.1:9000 soakroot soakrootsecret >/dev/null
mc mb --ignore-existing "local/$bucket" "local/$outside" "local/${public%%/*}" >/dev/null
mc anonymous set download "local/${public%%/*}" >/dev/null
echo "public object of the soak dry run" | "$dc" exec -T minio mc pipe "local/$public" >/dev/null
mc admin user add local "$user" "$pass" >/dev/null
policy="$(sed "s/BUCKET/$bucket/g" "$here/iam/soak-iam-policy.json")"
put_policy() { printf '%s' "$1" | "$dc" exec -T minio sh -c 'cat > /tmp/soak-policy.json && mc admin policy create local soak /tmp/soak-policy.json' >/dev/null 2>&1; }
if ! put_policy "$policy"; then
    # MinIO does not know every AWS action name: drop the ones it refuses (ACL calls are no-ops there)
    policy="$(printf '%s' "$policy" | jq '.Statement[].Action |= map(select(. != "s3:PutObjectAcl"))')"
    put_policy "$policy" || { echo "dry-setup: MinIO refused the policy" >&2; exit 1; }
    echo "dry-setup: s3:PutObjectAcl left out of the MinIO policy (not an action MinIO has)"
fi
mc admin policy attach local soak --user "$user" >/dev/null 2>&1 || true
mc admin user info local "$user" | sed -n 's/^PolicyName: /dry-setup: user policy: /p'
echo "dry-setup: done"
