# Connecting rs5cmd to storage

Checked against release v0.1.0.

## Quick setups

```bash
# AWS S3, credentials from a profile
rs5cmd --profile work --region eu-west-1 ls

# AWS S3, credentials from the environment
export AWS_ACCESS_KEY_ID=... AWS_SECRET_ACCESS_KEY=... AWS_REGION=eu-west-1
rs5cmd ls s3://my-bucket/

# MinIO, Ceph or another S3-compatible service
export AWS_ACCESS_KEY_ID=... AWS_SECRET_ACCESS_KEY=...
export AWS_REGION=us-east-1                        # set it even if the service ignores it
export AWS_ENDPOINT_URL=https://s3.example.com:9000
rs5cmd ls

# The same service behind a private certificate authority
rs5cmd --ca-certs-file /etc/ssl/private-ca.pem ls

# An existing s3cmd setup
rs5cmd --s3cfg ~/.s3cfg ls
```

## Where each setting comes from

For every setting, the first source that provides a value wins:

1. A command-line flag.
2. The environment variable bound to that flag: `AWS_ENDPOINT_URL` for `--endpoint-url`,
   `AWS_REGION` for `--region`.
3. The file given with `--s3cfg`.
4. `~/.rs5cmd`, which is read on every run when it exists.
5. For the endpoint only: `S3_ENDPOINT_URL`.
6. The AWS SDK defaults (shared config files, then `us-east-1` for the region).

Two settings behave differently when they come from a file (3 or 4):

- `no_verify_ssl = true` in a file cannot be switched back on from the command line. Edit the
  file to re-enable verification.
- `retry_count` from a file is used only while `--retry-count` is at its default of 10.

There is no flag to skip `~/.rs5cmd`. If results look wrong on a machine you did not set up,
check whether that file exists and what endpoint it names.

## Credentials

rs5cmd uses the AWS SDK default credential chain, in this order:

1. `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and optionally `AWS_SESSION_TOKEN`.
2. The shared files `~/.aws/credentials` and `~/.aws/config`. Choose a profile with
   `--profile NAME` or `AWS_PROFILE`. Point at another credentials file with
   `--credentials-file PATH` (it sets `AWS_SHARED_CREDENTIALS_FILE`).
3. Web identity, and container or instance metadata, as configured for the SDK. SSO (IAM
   Identity Center) profiles only in builds with the `sso` feature, which release binaries and
   packages leave out: there such a profile fails with an error that names the feature, and
   `eval "$(aws configure export-credentials --profile NAME --format env)"` puts its
   credentials in the environment instead.

Keys found in a `--s3cfg` file or in `~/.rs5cmd` are used only when the three environment
variables in step 1 are not already set.

There is no flag that takes a key directly. For anonymous access to a public bucket, give
`--no-sign-request`: no credentials are looked up and requests go unsigned.

## Endpoint and addressing

- `--endpoint-url URL` or `AWS_ENDPOINT_URL` selects an S3-compatible service. Include the
  scheme and, if needed, the port: `http://localhost:9000`, `https://s3.example.com`.
- `S3_ENDPOINT_URL` is also read, with the lowest priority. `cp --fast` does not accept it.
- With a custom endpoint, requests are path-style (`https://host/bucket/key`). Pass
  `--addressing-style virtual` for `https://bucket.host/key`. Against AWS the default is virtual.
- `--source-endpoint-url` / `--destination-endpoint-url` and `--source-region` /
  `--destination-region` set one side of a `cp`, `mv` or `sync`. When the two sides differ, the
  data is downloaded and uploaded by the client; there is no server-side copy between services.
  Both sides use the same credentials.
- `--use-dualstack-endpoint` and `--use-fips-endpoint` choose AWS endpoint variants. They do
  nothing with a custom endpoint.

## Region

`--region` or `AWS_REGION`, else the region of the active profile. With none of these, rs5cmd
asks the bucket: one `HeadBucket` request (once per bucket and endpoint in a run), whose
`x-amz-bucket-region` answer it uses, else `us-east-1`, as s5cmd does. The instance metadata
service is not asked for a region. Setting the region saves the extra request.

## TLS

- `--ca-certs-file bundle.pem` adds the certificates in a PEM file to the trusted set. Use it
  for a private or self-signed CA.
- `--no-verify-ssl` disables certificate and host name verification. With it,
  `--ca-certs-file` is ignored. Use it only for a test endpoint you trust on a network you trust.
- With an `http://` endpoint nothing is encrypted in transit.
- `cp --fast` trusts only the public CA set. It honours `--no-verify-ssl`; with
  `--ca-certs-file` the command runs on the default engine.

## Proxy

`--proxy URL` (short `-x`) accepts `socks5://`, `socks5h://`, `http://` and `https://`, with
optional `user:pass@`. Without the flag, the first of `ALL_PROXY`, `HTTPS_PROXY`, `HTTP_PROXY`
that is set is used (upper or lower case). `cp --fast` with `--proxy` runs on the default
engine; the fast engine does not use the proxy environment variables.

## Bandwidth

`--limit-upload RATE` and `--limit-download RATE` cap the total rate of one command in bytes per
second (`512KB`, `10MB`, `1GB`). `--limitrate RATE` caps both directions. With any of the
three, `cp --fast` runs on the default engine.

## Using an s3cmd configuration

`--s3cfg PATH` applies an s3cmd config file for one run. Section headers are not distinguished:
the keys of all sections are merged, which is fine for the usual single `[default]` section.

| `.s3cfg` key | Becomes |
|---|---|
| `access_key`, `secret_key`, `access_token` | Credentials |
| `host_base`, `use_https` (default true) | Endpoint URL `https://<host_base>` or `http://<host_base>` |
| `host_bucket` | Starts with `%(bucket)s`: virtual addressing. Otherwise path |
| `bucket_location` or `region` | Region. `US` means `us-east-1`, `EU` means `eu-west-1` |
| `check_ssl_certificate = False` or `check_ssl_hostname = False` | `--no-verify-ssl` |
| `ca_certs_file` | `--ca-certs-file` |
| `proxy_host`, `proxy_port` (default 3128) | `--proxy http://host:port` |
| `max_retries` | `--retry-count` |
| `limitrate` | `--limitrate` |
| `requester_pays` | `--request-payer requester` |

These keys are recognised but not applied, and each prints a warning: `signature_v2` (rs5cmd
signs with SigV4 only), `storage_class`, `server_side_encryption`, `kms_key`, `encrypt`. Other
keys are ignored.

### Making it permanent: `import-s3cfg`

```bash
rs5cmd import-s3cfg ~/.s3cfg
```

This writes two files:

- `~/.rs5cmd` with the connection settings and no secrets. Change the location with
  `--out FILE`, but only `~/.rs5cmd` is loaded automatically.
- `~/.aws/credentials` (mode 0600) with the keys. It **replaces the `[default]` profile** and
  keeps the other profiles. Pass `--profile NAME` to write a different profile (then run rs5cmd
  with `--profile NAME`), or `--no-credentials` to leave the file alone.

Ask the user before running it on their account, because it changes the default AWS profile
that other tools read.

### `~/.rs5cmd` format

```ini
# rs5cmd config (generated by `import-s3cfg`)
[rs5cmd]
endpoint_url = https://storage.example.com:9000
region = us-east-1
addressing_style = path
no_verify_ssl = true
proxy = http://proxy.example.com:8080
retry_count = 4
limitrate = 2000000
```

Other keys it understands: `ca_certs_file`, `request_payer`, and `access_key`, `secret_key`,
`session_token` (prefer the AWS credentials file for those). The file can be written by hand.

## Checking a connection

```bash
rs5cmd ls                         # lists buckets: proves endpoint, TLS and credentials
rs5cmd head s3://bucket           # exit code 0 when the bucket is reachable
RUST_LOG=debug rs5cmd ls 2>debug.log     # request-level detail on stderr
```

Typical failures. `dispatch failure` means the request never got an answer; the text after it
names the reason.

| Message contains | Likely cause |
|---|---|
| `no providers in chain provided credentials` | No credentials found. Set the variables or a profile |
| `tcp connect error: Connection refused` | Wrong endpoint host, port or scheme, or the service is down |
| `invalid peer certificate: UnknownIssuer` | Private CA: pass `--ca-certs-file` |
| `SignatureDoesNotMatch` (error code from the service) | Wrong secret key or region, or something rewrote the request |
| `PermanentRedirect` (error code from AWS) | The bucket lives in another region: pass `--region` |
| Several seconds of metadata warnings at start | No region configured |
