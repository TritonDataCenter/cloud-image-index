Install or update in a SmartOS global zone:

```
curl -fsSL https://github.com/@REPOSITORY@/releases/download/@TAG@/install.sh | CLOUD_IMAGE_RELEASE=@TAG@ bash
```

The latest release is always:

```
curl -fsSL https://github.com/@REPOSITORY@/releases/latest/download/install.sh | bash
```

**CA certificates.** The install checks every download over TLS, so `curl` needs a CA bundle. Use the one from pkgsrc tools:

```
export CURL_CA_BUNDLE=/opt/tools/etc/openssl/certs/ca-certificates.crt
```

If that file is missing, `/opt/tools/sbin/mozilla-rootcerts install` creates it (from pkgsrc tools' `mozilla-rootcerts` package).

*Without pkgsrc tools (workaround):* the SmartOS platform does not yet ship a CA bundle; [illumos-extra#192](https://github.com/TritonDataCenter/illumos-extra/pull/192) adds one. Until it does, the root certificates built into the platform's Node.js can check this one download:

```
/usr/node/bin/node -e 'var s=require("fs").readFileSync(process.execPath).toString("binary");process.stdout.write((s.match(/-----BEGIN CERTIFICATE-----\n[A-Za-z0-9+\/=\n]+-----END CERTIFICATE-----/g)||[]).join("\n")+"\n")' > /tmp/cloud-image-roots.pem
curl --cacert /tmp/cloud-image-roots.pem -fsSL https://github.com/@REPOSITORY@/releases/latest/download/install.sh | CURL_CA_BUNDLE=/tmp/cloud-image-roots.pem bash
```

These certificates date from 2014 (Node.js 0.10): use them only for this download, and expect the workaround to stop working if GitHub changes its certificates.

**After the install, neither is needed.** `cloud-image` carries Mozilla's root certificates inside the binary, as they were when it was built, and trusts only those, never the system's: they protect everything it fetches, the index, the vendors' checksum files and images, and `cloud-image self-update`, which brings newer certificates with each release.
