"""Independent S3 checker for the soak: a small SigV4 client on the standard library.

It verifies what the measured clients did (HEAD, listings, versions, incomplete multipart
uploads), creates the few objects no client can create (a key that ends in "/"), tests the
endpoint's checksum handling (S11) and does the final cleanup. It is never measured, and it
shares no code with rs5cmd or s5cmd.
"""
import base64
import datetime
import hashlib
import hmac
import http.client
import os
import ssl
import time
import urllib.parse
import xml.etree.ElementTree as ET
import zlib


def _q(s, safe="-_.~"):
    return urllib.parse.quote(s, safe=safe)


class S3Error(Exception):
    def __init__(self, status, code, message, body=b""):
        super().__init__(f"{status} {code}: {message}")
        self.status, self.code, self.message, self.body = status, code, message, body


def _strip_ns(root):
    for el in root.iter():
        if "}" in el.tag:
            el.tag = el.tag.split("}", 1)[1]
    return root


def parse_error(status, body):
    code = message = ""
    try:
        root = _strip_ns(ET.fromstring(body))
        code = root.findtext("Code") or ""
        message = root.findtext("Message") or ""
    except ET.ParseError:
        pass
    return S3Error(status, code, message, body)


# --- checksums (pure Python: about 16 MiB/s for CRC32C, fine up to some tens of MiB) --------

def _table(poly, bits):
    mask = (1 << bits) - 1
    t = []
    for i in range(256):
        c = i
        for _ in range(8):
            c = (c >> 1) ^ poly if c & 1 else c >> 1
        t.append(c & mask)
    return t


_CRC32C = _table(0x82F63B78, 32)
_CRC64NVME = _table(0x9A6C9329AC4BC9B5, 64)


def crc32c(data):
    c = 0xFFFFFFFF
    for b in data:
        c = _CRC32C[(c ^ b) & 0xFF] ^ (c >> 8)
    return c ^ 0xFFFFFFFF


def crc32c_file(path, offset=0, length=None):
    """CRC32C of a file or of one part of it."""
    c, t = 0xFFFFFFFF, _CRC32C
    with open(path, "rb") as f:
        f.seek(offset)
        left = os.path.getsize(path) - offset if length is None else length
        while left > 0:
            data = f.read(min(left, 1 << 20))
            if not data:
                break
            for b in data:
                c = t[(c ^ b) & 0xFF] ^ (c >> 8)
            left -= len(data)
    return c ^ 0xFFFFFFFF


def crc32c_composite(path, part_size):
    """What S3 reports for a multipart upload whose parts carried CRC32C: the CRC32C of the
    parts' raw checksums, with the number of parts."""
    size, raw, n = os.path.getsize(path), b"", 0
    for off in range(0, max(size, 1), part_size):
        raw += crc32c_file(path, off, min(part_size, size - off)).to_bytes(4, "big")
        n += 1
    return f"{b64(crc32c(raw), 4)}-{n}"


def crc64nvme(data):
    c = 0xFFFFFFFFFFFFFFFF
    for b in data:
        c = _CRC64NVME[(c ^ b) & 0xFF] ^ (c >> 8)
    return c ^ 0xFFFFFFFFFFFFFFFF


assert crc32c(b"123456789") == 0xE3069283
assert crc64nvme(b"123456789") == 0xAE8B14860A799888


def b64(n, width):
    return base64.b64encode(n.to_bytes(width, "big")).decode()


def checksums(data):
    """The base64 values S3 reports for a whole small object."""
    return {
        "crc32": b64(zlib.crc32(data) & 0xFFFFFFFF, 4),
        "crc32c": b64(crc32c(data), 4),
        "crc64nvme": b64(crc64nvme(data), 8),
        "sha256": base64.b64encode(hashlib.sha256(data).digest()).decode(),
        "sha1": base64.b64encode(hashlib.sha1(data).digest()).decode(),
    }


# --- client ----------------------------------------------------------------------------------

class S3:
    def __init__(self, access_key, secret_key, region, endpoint_url=None, addressing=None,
                 session_token=None, ca_file=None, verify=True):
        self.ak, self.sk, self.token, self.region = access_key, secret_key, session_token, region
        if endpoint_url:
            u = urllib.parse.urlsplit(endpoint_url)
            self.scheme, self.host = u.scheme, u.netloc
            self.addressing = addressing or "path"
        else:
            self.scheme, self.host = "https", f"s3.{region}.amazonaws.com"
            self.addressing = addressing or "virtual"
        self.ctx = None
        if self.scheme == "https":
            self.ctx = ssl.create_default_context(cafile=ca_file)
            if not verify:
                self.ctx.check_hostname = False
                self.ctx.verify_mode = ssl.CERT_NONE
        self._conns = {}
        self.requests = 0

    def _conn(self, host, fresh=False):
        c = self._conns.get(host)
        if c is not None and fresh:
            c.close()
            c = None
        if c is None:
            if self.scheme == "https":
                c = http.client.HTTPSConnection(host, timeout=120, context=self.ctx)
            else:
                c = http.client.HTTPConnection(host, timeout=120)
            self._conns[host] = c
        return c

    def close(self):
        for c in self._conns.values():
            c.close()
        self._conns.clear()

    def _target(self, bucket, key):
        if not bucket:
            return self.host, "/"
        if self.addressing == "virtual":
            return f"{bucket}.{self.host}", "/" + key
        return self.host, f"/{bucket}/{key}" if key else f"/{bucket}"

    def request(self, method, bucket="", key="", query=None, headers=None, body=b"",
                payload_hash=None, unsigned=False, ok=(200, 204, 206), retry=True):
        """One request. Returns (status, headers, body). With ok=None nothing is raised."""
        host, path = self._target(bucket, key)
        uri = _q(path, safe="/-_.~")
        cq = "&".join(f"{_q(k)}={_q(v)}" for k, v in sorted((query or {}).items()))
        hdrs = {k.lower(): str(v).strip() for k, v in (headers or {}).items()}
        hdrs["host"] = host
        if not unsigned:
            now = datetime.datetime.now(datetime.timezone.utc)
            amzdate, datestamp = now.strftime("%Y%m%dT%H%M%SZ"), now.strftime("%Y%m%d")
            ph = payload_hash or hashlib.sha256(body).hexdigest()
            hdrs["x-amz-date"] = amzdate
            hdrs["x-amz-content-sha256"] = ph
            if self.token:
                hdrs["x-amz-security-token"] = self.token
            names = sorted(hdrs)
            creq = "\n".join([method, uri, cq, "".join(f"{n}:{' '.join(hdrs[n].split())}\n" for n in names),
                              ";".join(names), ph])
            scope = f"{datestamp}/{self.region}/s3/aws4_request"
            sts = "\n".join(["AWS4-HMAC-SHA256", amzdate, scope, hashlib.sha256(creq.encode()).hexdigest()])
            k = ("AWS4" + self.sk).encode()
            for part in (datestamp, self.region, "s3", "aws4_request"):
                k = hmac.new(k, part.encode(), hashlib.sha256).digest()
            sig = hmac.new(k, sts.encode(), hashlib.sha256).hexdigest()
            hdrs["authorization"] = (f"AWS4-HMAC-SHA256 Credential={self.ak}/{scope}, "
                                     f"SignedHeaders={';'.join(names)}, Signature={sig}")
        url = uri + ("?" + cq if cq else "")
        attempt = 0
        while True:
            attempt += 1
            try:
                c = self._conn(host, fresh=attempt > 1)
                c.request(method, url, body=body, headers=hdrs)
                r = c.getresponse()
                data = r.read()
                status, rh = r.status, {k.lower(): v for k, v in r.getheaders()}
            except (OSError, http.client.HTTPException):
                if attempt >= 4:
                    raise
                time.sleep(0.5 * attempt)
                continue
            self.requests += 1
            if retry and status in (500, 502, 503, 504) and attempt < 5:
                time.sleep(attempt)
                continue
            if ok is not None and status not in ok:
                raise parse_error(status, data)
            return status, rh, data

    # --- objects ---
    def head_object(self, bucket, key, version_id=None, part_number=None, checksum=False):
        q, h = {}, {}
        if version_id:
            q["versionId"] = version_id
        if part_number:
            q["partNumber"] = str(part_number)
        if checksum:
            h["x-amz-checksum-mode"] = "ENABLED"
        status, rh, _ = self.request("HEAD", bucket, key, query=q, headers=h, ok=None)
        if status not in (200, 206):
            raise S3Error(status, "NotFound" if status == 404 else str(status), "HEAD failed")
        return rh

    def exists(self, bucket, key):
        try:
            self.head_object(bucket, key)
            return True
        except S3Error as e:
            if e.status == 404:
                return False
            raise

    def get_object(self, bucket, key, version_id=None, unsigned=False):
        q = {"versionId": version_id} if version_id else {}
        return self.request("GET", bucket, key, query=q, unsigned=unsigned)[2]

    def put_object(self, bucket, key, body, headers=None):
        return self.request("PUT", bucket, key, headers=headers, body=body)[1]

    def put_object_trailer(self, bucket, key, data, crc32c_b64):
        """aws-chunked upload with an unsigned payload and a CRC32C trailer. Returns (status, code)."""
        body = (f"{len(data):x}\r\n".encode() + data + b"\r\n0\r\nx-amz-checksum-crc32c:"
                + crc32c_b64.encode() + b"\r\n\r\n")
        h = {"content-encoding": "aws-chunked", "x-amz-decoded-content-length": str(len(data)),
             "x-amz-trailer": "x-amz-checksum-crc32c", "x-amz-sdk-checksum-algorithm": "CRC32C"}
        status, _, rb = self.request("PUT", bucket, key, headers=h, body=body, ok=None, retry=False,
                                     payload_hash="STREAMING-UNSIGNED-PAYLOAD-TRAILER")
        return status, parse_error(status, rb).code if status >= 300 else ""

    def delete_object(self, bucket, key, version_id=None):
        q = {"versionId": version_id} if version_id else {}
        self.request("DELETE", bucket, key, query=q)

    def delete_objects(self, bucket, items):
        """items: keys or (key, version_id) pairs, at most 1000. Returns [(key, code)] of errors."""
        root = ET.Element("Delete")
        ET.SubElement(root, "Quiet").text = "true"
        for it in items:
            key, vid = it if isinstance(it, tuple) else (it, None)
            o = ET.SubElement(root, "Object")
            ET.SubElement(o, "Key").text = key
            if vid:
                ET.SubElement(o, "VersionId").text = vid
        body = ET.tostring(root)
        md5 = base64.b64encode(hashlib.md5(body).digest()).decode()
        data = self.request("POST", bucket, query={"delete": ""}, body=body,
                            headers={"content-md5": md5, "content-type": "application/xml"})[2]
        errs = []
        if data.strip():
            for e in _strip_ns(ET.fromstring(data)).findall("Error"):
                errs.append((e.findtext("Key"), e.findtext("Code")))
        return errs

    def list_objects(self, bucket, prefix="", delimiter=None):
        """All objects under a prefix (ListObjectsV2, url-encoded keys)."""
        token = None
        while True:
            q = {"list-type": "2", "prefix": prefix, "encoding-type": "url", "max-keys": "1000"}
            if delimiter:
                q["delimiter"] = delimiter
            if token:
                q["continuation-token"] = token
            root = _strip_ns(ET.fromstring(self.request("GET", bucket, query=q)[2]))
            for c in root.findall("Contents"):
                yield {"key": urllib.parse.unquote_plus(c.findtext("Key")),
                       "size": int(c.findtext("Size")),
                       "etag": (c.findtext("ETag") or "").strip('"'),
                       "storage_class": c.findtext("StorageClass") or ""}
            if root.findtext("IsTruncated") != "true":
                return
            token = root.findtext("NextContinuationToken")

    def list_versions(self, bucket, prefix=""):
        out, km, vm = [], None, None
        while True:
            q = {"versions": "", "prefix": prefix, "encoding-type": "url"}
            if km:
                q["key-marker"] = km
            if vm:
                q["version-id-marker"] = vm
            root = _strip_ns(ET.fromstring(self.request("GET", bucket, query=q)[2]))
            for el in root:
                if el.tag in ("Version", "DeleteMarker"):
                    out.append({"key": urllib.parse.unquote_plus(el.findtext("Key")),
                                "version_id": el.findtext("VersionId"),
                                "is_latest": el.findtext("IsLatest") == "true",
                                "delete_marker": el.tag == "DeleteMarker",
                                "size": int(el.findtext("Size") or 0),
                                "etag": (el.findtext("ETag") or "").strip('"')})
            if root.findtext("IsTruncated") != "true":
                return out
            km = urllib.parse.unquote_plus(root.findtext("NextKeyMarker") or "")
            vm = root.findtext("NextVersionIdMarker")

    def list_multipart_uploads(self, bucket, prefix=""):
        out, km, um = [], None, None
        while True:
            q = {"uploads": "", "prefix": prefix}
            if km:
                q["key-marker"] = km
            if um:
                q["upload-id-marker"] = um
            root = _strip_ns(ET.fromstring(self.request("GET", bucket, query=q)[2]))
            for u in root.findall("Upload"):
                out.append({"key": u.findtext("Key"), "upload_id": u.findtext("UploadId"),
                            "initiated": u.findtext("Initiated")})
            if root.findtext("IsTruncated") != "true":
                return out
            km, um = root.findtext("NextKeyMarker"), root.findtext("NextUploadIdMarker")

    def abort_multipart_upload(self, bucket, key, upload_id):
        self.request("DELETE", bucket, key, query={"uploadId": upload_id})

    # --- buckets ---
    def head_bucket(self, bucket):
        return self.request("HEAD", bucket, ok=None)[0]

    def create_bucket(self, bucket):
        body = b""
        if self.region != "us-east-1":
            body = (b'<CreateBucketConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">'
                    b"<LocationConstraint>" + self.region.encode() +
                    b"</LocationConstraint></CreateBucketConfiguration>")
        self.request("PUT", bucket, body=body)

    def delete_bucket(self, bucket):
        self.request("DELETE", bucket)

    def get_bucket_versioning(self, bucket):
        data = self.request("GET", bucket, query={"versioning": ""})[2]
        return _strip_ns(ET.fromstring(data)).findtext("Status") or ""

    def list_buckets(self):
        data = self.request("GET")[2]
        return [b.findtext("Name") for b in _strip_ns(ET.fromstring(data)).iter("Bucket")]
