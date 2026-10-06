# cloud-image-index design

Status: draft. Decisions below were made in design discussion on
2026-10-05 and 2026-10-06. Decided items that are not implemented yet
are marked **Not built yet**; undecided ones are marked **Open** or
listed under "Open questions".

## Motivation

Finding the current cloud image for an operating system means knowing
each vendor's own way of publishing it: Canonical's Simple Streams,
Fedora's `releases.json`, Debian's apt `Release` files, HTML directory
listings (FreeBSD, Rocky, CentOS Stream), an HTML table (Oracle), the
GitHub Releases API (Talos, OpenBSD), MirrorCache JSON (openSUSE), and
so on. These formats change, and new OS releases appear, on the
vendors' schedules rather than ours.

That knowledge currently lives in two diverging copies inside Triton
tooling:

- `monitor-reef`: `cli/tritonadm/src/commands/image/nocloud/`
  (`tritonadm image fetch-nocloud`), including VMDK decoding.
- A second copy in another Triton repository, lifted from the above and
  extended with `list_versions`, `Vendor::ALL`, and release-token
  validation; VMDK decoding is disabled there.

Every client that embeds this logic must be updated when a vendor
changes. Moving discovery into a published index means the index is
updated once and every client benefits unmodified. The duplication
across the two monorepos is itself the signal that this code needs its
own home; it moves here.

## Goals

- Publish as much accurate, reusable information about vendor cloud
  images as we can: exact builds, artifact URLs, sizes, checksums and
  where the vendor publishes them, signatures, release aliases, and
  cross-references to osinfo-db.
- Define the format rigorously as a Dropshot API trait and publish the
  generated OpenAPI document, so that other hypervisors, and possibly OS
  vendors, can consume or contribute to it.
- No servers to operate: static files in git, served by GitHub Pages.

## Non-goals (for the first stage)

- Clients. How a client uses the index, and how much it trusts it, is
  designed after the index exists. A client may be as trusting or as
  paranoid as it likes; we may, for example, restrict our own clients
  to vendor domains we trust and require TLS.
- Hosting or mirroring image bits. Images are always fetched from the
  vendor.
- Converting images (qcow2 to zvol, IMGAPI manifests, etc.). The
  conversion pipeline also moves into this repository, but it is a
  client concern and is separate from the index format.

## Architecture

```
vendor sites --(scheduled GitHub Action runs Rust resolvers)--> JSON tree in git
JSON tree in git --(GitHub Pages)--> clients
```

- **Resolvers**: Rust code, one module per vendor, derived from the
  existing `nocloud` vendor profiles. Vendor-specific logic stays in
  reviewed code rather than data files.
- **Scheduled GitHub Action**: runs the resolvers and writes the tree,
  committing every change directly. **Not built yet**: structural
  changes opening a pull request instead (see "Update policy").
- **GitHub Pages** serves the tree. The first iteration publishes at
  `tritondatacenter.github.io/cloud-image-index`. Before any long-lived
  client (such as one shipped in a platform image) is released, the
  index moves to a custom domain we control, so that such clients are
  not tied to GitHub. Clients must never use `raw.githubusercontent.com`.
  Clients take the base URL as configuration rather than compiling it
  in.
- **Dropshot API trait**: defines every path and type, and generates
  the OpenAPI document published as `v1/openapi.json`. **Not built
  yet**: a small reference server implementing the trait by reading the
  tree from disk, to test that a generated client and the static files
  agree and to serve a mirror.
- **Offline / air-gapped use**: a clone of the repository is a complete
  mirror; clients take a base URL, which may be `file://`.

## File tree

No path is both a directory and a file. Aliases are real files, not
symlinks. Dropshot does not allow a literal path segment and a variable
one at the same level (it rejects `/v1/index.json` beside
`/v1/{distro}/...`), so literal names never sit beside `{distro}` or
`{release}`:

```
index.html                                         # landing page for people (not in the OpenAPI document)
v1/index.json                                      # list of distros
v1/openapi.json                                    # the OpenAPI document describing this tree
v1/distros/<distro>/index.json                     # releases, aliases, osinfo ids
v1/distros/<distro>/aliases/latest.json            # copy of the aliased release's index.json
v1/distros/<distro>/aliases/lts.json
v1/distros/<distro>/aliases/dev.json
v1/distros/<distro>/releases/<release>/index.json  # current builds in the vendor's main tree
v1/distros/<distro>/releases/<release>/archive.json # builds available only from a vendor vault
```

The types and paths are defined in `apis/cloud-image-index-api`.
`examples/` holds a hand-built tree (Ubuntu, Rocky, Talos; x86_64 only,
one build per release) with values read from the vendors on
2026-10-05; its tests check that every file sits at an API path,
round-trips through its type, and agrees with the distro index.

`index.html` is an unpublished endpoint of the API: part of the tree,
so the tree checks cover it, but left out of the OpenAPI document
because it is not part of the format. Its source is
`apis/cloud-image-index-api/src/index.html`; the generator writes it
into every tree, and `examples/index.html` must be an exact copy. It
links to the files by relative paths, so it works at any base URL.

`v1/openapi.json` is itself an endpoint of the API, so the document
lists its own path, and every tree carries the spec of the version that
wrote it. The generator writes it into every tree, and the tree checks
require it to equal the document the current code generates.
`examples/v1/openapi.json` is the committed copy: a change to the API
fails the tests until that copy is refreshed
(`cloud-image-index-generate openapi > examples/v1/openapi.json`), so
every schema change shows up as a diff in review. The check catches any
change; it does not judge whether a change is compatible (see "Schema
evolution").

## Data model

### Identity

An artifact is identified by:

```
distro / release / build / variant / arch / format
ubuntu / noble   / 20260926 / server / x86_64 / qcow2
```

- **distro**: our own short name (`ubuntu`, `rocky`, `centos-stream`).
- **release**: the vendor's major release or series (`noble`, `9`,
  `trixie`). For distros with point releases, the point release is a
  field on the build, not part of the release (first approximation;
  expected to be revisited).
- **build**: the vendor's build id or serial.
- **arch**: osinfo-db architecture names (`x86_64`, `aarch64`), not
  vendor spellings such as Ubuntu's `amd64`.
- **variant**: the vendor's image flavour (Debian `genericcloud` /
  `generic` / `nocloud`; Rocky Base / LVM; Ubuntu server / minimal).
  All variants are listed, and the vendor's default is marked.

Identity never includes the URL: a build moved from the main tree to a
vault keeps its identity and gains a different location.

### Builds and artifacts

Summary of the fields; the Rust types in `apis/cloud-image-index-api`
are authoritative:

- Identity: distro, release, build, variant, arch, format; point
  release where applicable; aliases this release currently holds.
- Artifact: `url`, `size`, `format`, `compression`, `arch`, firmware
  (UEFI / BIOS), cloud-init datasources, whether SSH keys can be
  injected, vendor `published_at`.
- Locations: list of `{url, kind}` where kind is `primary`, `vault` or
  `mirror`, plus whether the vendor redirects the URL to a different
  host (e.g. `download.opensuse.org` redirects to mirrors such as
  `mirrors.rit.edu`; GitHub release URLs redirect to GitHub's asset
  host). The target host is not recorded: mirror redirectors pick a
  different one per request, which would change the index on every
  run.
- Integrity, as much as the vendor offers:
  - the hash value(s) as read from the vendor;
  - the vendor checksum file URL, its format (GNU `<hex>  file`, BSD
    `SHA256 (file) = hex`, or bare hash) and algorithm, so a client can
    cross-check against the vendor;
  - signature URL and type, and where to find the vendor's key;
  - when the vendor publishes no hash: the HTTP `Content-Length`,
    `Last-Modified` and `ETag` (flagged as opaque, not a hash).
- Bookkeeping: `first_seen`. No "last checked" timestamps in data files,
  so that polls without changes produce no commits.

### Release aliases

- `latest`: the newest generally-available release. Never a
  pre-release.
- `lts`: the newest release the vendor itself labels long-term support
  (e.g. Ubuntu LTS, OmniOS `lts`). Only published where the vendor uses
  that label; otherwise the file does not exist.
- `dev`: the vendor's development channel or newest pre-release
  (OmniOS `bloody`, Fedora's beta, openSUSE's RC). The distro index
  records the vendor's name for it in `dev_channel`. Debian has none
  yet (see "Generator status").

The resolvers report these labels themselves (`lts`, `dev`, and
whether an entry is a moving channel); the generator's rules are
vendor-agnostic.

Vendor-named suites such as Debian `stable` / `oldstable` remain
ordinary release names rather than aliases.

Each release in `v1/distros/<distro>/index.json` lists the aliases it
holds.

### osinfo-db cross-references

**Status: the fields exist but are not populated** (decided
2026-10-06). The generator always leaves `Release.osinfo` and
`Build.osinfo` empty until the source of the mapping is chosen.

Why the fields exist: an osinfo-db id (`http://ubuntu.com/ubuntu/24.04`)
and short id (`ubuntu24.04`) let the index's consumers reuse the wider
virtualization ecosystem. `virt-install --osinfo` and virt-manager take
these ids directly; tools can look up minimum RAM and disk, firmware
and device support in osinfo-db instead of us publishing them; and
referring to osinfo-db's ids gives other projects a shared vocabulary
with this index. A release carries the closest id, and a build may
override it where osinfo-db has a more specific point-release entry
(e.g. Rocky 8.6).

Why it is not trivial: osinfo-db's granularity varies by distro (Alma
per major; Rocky partly per point release, plus major and `-unknown`
catch-all entries; OmniOS only `bloody-rolling`; no Talos entry), and
new releases often appear before osinfo-db's next release (about twice
a year). The mapping therefore needs both per-vendor rules and a list
of the ids that actually exist.

Options for where the mapping comes from:

1. A table maintained by hand in each resolver. Simple, but it goes
   stale silently as releases arrive.
2. The generator reads osinfo-db from GitLab on each run. Always
   current, but adds a host the daily job depends on.
3. A copy of osinfo-db's list of ids (id and short id only) checked into
   this repository and refreshed periodically, e.g. by a workflow that
   opens a pull request. Each resolver supplies a rule (Rocky major N ->
   `http://rockylinux.org/rocky/N`); the generator publishes an id only
   if the list has it, otherwise the matching `-unknown` entry, otherwise
   nothing. Checkable, offline, and publishes only ids that exist.

Option 3 was the leading candidate. Before choosing it, confirm that
checking in a list of osinfo-db's ids is compatible with osinfo-db's
license (GPL-2.0-or-later) and this repository's (MPL-2.0); option 2
avoids the question by only reading the data at run time.

## Schema evolution

Clients generated from the OpenAPI document (progenitor/typify) must
keep working while the index changes under them, until they are
regenerated. `tests/typify-client` generates client types from the
spec with typify and checks:

- Extra fields are ignored (the spec never sets
  `additionalProperties: false`). Adding a field is safe.
- Only what a minimal, trusting client needs to find an image and
  ingest it is required; everything else is optional (`Option`, or a
  list with `#[serde(default)]`), so it can be dropped later without
  breaking clients. Required: `DistroList.distros`; a distro's `id`,
  `os_family` and `releases`; a release's `id`; a build list's
  `distro`, `release` and `builds`; a build's `build` and `artifacts`;
  an artifact's `arch`, `format`, `compression` and `locations`; a
  location's `url`. Inside optional structures the fields that make
  them usable stay required (a digest's `algorithm` and `value`, a
  signature's `url`, `kind` and `signs`, a checksum source's `url` and
  `format`). The index itself still fills every field it can, and the
  tree checks still enforce its own rules (e.g. current builds have a
  location marked `primary`).
- Open enums, whose value sets are expected to grow (`ImageFormat`,
  `Compression`, `ChecksumFormat`, `SignatureKind`, `Alias`), accept
  and keep values a client does not know. In Rust they have an
  `Other(String)` catch-all; in the schema they are `oneOf` a known
  value or a string matching a pattern that excludes the known values
  (typify turns a `not: {enum}` branch into an empty enum, so `not` is
  not used). Generated clients need the `regress` crate for the pattern.
- Closed enums (`OsFamily`, `LocationKind`, `DigestAlgorithm`,
  `Firmware`) reject unknown values; adding a value to one is a breaking
  change.
- The index itself never publishes an `Other` value: the tree checks
  reject one, so typos are caught rather than published.

Breaking changes go to a new `v2/` tree published alongside `v1/`.

## Update policy

- **Auto-commit**: a new build of an already-listed release, from an
  already-trusted host.
- **Pull request**: a new release, new host, new distro, or format
  change. The list of trusted hosts lives in the repository.
  **Not built yet**: today every change is committed directly and
  there is no list of trusted hosts.
- **Removal**: a build is removed when the vendor no longer serves it,
  defined as a definite 404/410 on two consecutive runs. Timeouts, 5xx
  and TLS failures fail the vendor, which keeps its previous files,
  instead of removing anything. Git history is the record of removed
  builds. **Not built yet**: the second run; a single 404/410 removes
  the build (see "Generator status").
- **Timeouts**: every request has a 30-second connect timeout and a
  2-minute total timeout. A timeout is a transient failure.
- **Resolver errors**: a resolver that fails on one release with a
  404/410 from the vendor leaves that release out. Any other resolver
  error (no response, a server error, or a vendor page the resolver
  cannot understand, which is how a format change shows up) fails the
  whole vendor: its previous files are kept and its job goes red. So
  does anything the release rules cannot place: a supported entry whose
  token is not valid, a channel that does not say which release it
  resolved to, or a vendor list that could only be read in part
  (Debian's suites). Dropping such a release instead could move an
  alias, such as `latest`, without anyone noticing.
- **Vaults**: where a vendor has a vault or archive (e.g.
  `vault.almalinux.org`), the resolver checks it, and a build that has
  moved there is listed in `archive.json`. The archive lists only the
  final build of each end-of-life point release, not every historical
  build.
  **Open** for the vault work (2026-10-06):
  - The generator's model of a distro is its releases' current build
    lists only: `write_tree` never writes `archive.json`, and carrying a
    vendor over from the previous tree does not read it. Archives must
    be added to that model before any are produced, or they would be
    dropped on the next run.
  - The distro index lists only current releases, so an entirely
    retired release (e.g. a major moved wholly to the vault) would have
    an `archive.json` that nothing points to. Retired releases need to
    be listed, e.g. with a flag on the release or a separate list.
- **Failures**: a failed resolver run opens an issue. Recorded vendor
  responses are kept as test fixtures. **Not built yet**: today a
  failure shows only as a red job, and tests use small inline excerpts
  of vendor responses.

## Generator commands

The generator's commands map onto a CI matrix with one job per vendor,
so a vendor's failure shows as its own failed job and never blocks the
others:

```
cloud-image-index-generate vendors [--json]          # the matrix
cloud-image-index-generate vendor <name> --previous <site> --out <fragment>
cloud-image-index-generate assemble --site <site> <fragment>...
cloud-image-index-generate openapi                   # print v1/openapi.json
```

A vendor job writes a fragment: a small, valid tree holding only that
vendor. If the vendor fails it writes nothing and exits non-zero. The
assemble job replaces each fragment's vendor in the site tree, keeps
every other vendor as it was, rebuilds `v1/index.json` and validates
the whole tree; if no vendor produced a fragment, the assemble job
fails rather than publish nothing. `scripts/run-matrix-locally.sh [site] [work]` runs the
same commands locally (vendor jobs in parallel, one log each).
`cloud-image-index-generate all --site <site>` does everything in one
process.

`.github/workflows/index.yml` runs these daily (05:17 UTC) and on
demand ("Run workflow", optionally for a single vendor): a build job
compiles the generator once and lists the vendors, one matrix job per
vendor writes a fragment (`fail-fast: false`, so a vendor's failure
shows as its own red job), then one assemble job merges the fragments
into `site/`, commits it if it changed, and a deploy job publishes
`site/` to GitHub Pages. The published site is exactly the committed
`site/` tree; there is no deploy-only data. Actions are pinned to
release commit SHAs.

`.github/workflows/ci.yml` runs `cargo fmt --check`, clippy (warnings
as errors) and the tests on every pull request and on pushes to `main`
that touch more than `site/`. It does not check `site/`, which keeps
the previous version's `v1/openapi.json` after an API change until the
next index run rewrites it.

`site/` may hold only files the API defines: the generator refuses a
previous tree with anything else in it, so every job would fail. Pages
deployed from Actions needs neither `.nojekyll` nor `CNAME`; a custom
domain is set in the repository settings.

One-time setup, in the repository settings:

- Set Pages' source to "GitHub Actions" before the first push: once
  `main` exists, the scheduled run can fire and its deploy job fails
  while Pages is off. The first deployment can come from a manual run.
- Optionally set the default workflow permissions to read; the
  workflows request what they need.
- Branch rules on `main` must let the workflow push: the assemble job
  commits `site/` straight to `main`. Don't require the `ci` check on
  `main` without a bypass for GitHub Actions, since the bot's `site/`
  commits never trigger it.

## Generator status

`tools/generate` (`cloud-image-index-generate`) runs the resolvers in
`libs/resolvers` (carried over from that second copy) and writes the
tree. It is a first approximation. Known gaps, as of a full run on 2026-10-05:

- One build per release (the vendor's current one) and one artifact
  (the profile's default variant, x86_64). No vault archives.
- Every digest records the vendor document it came from. Signatures
  are published where the vendor serves them (Ubuntu, Alma, Rocky 9/10,
  Alpine, Fedora); signing-key URLs are not yet known. EOL dates come
  only from Alpine and Ubuntu feeds; publish dates only from GitHub
  (OpenBSD, Talos `latest`); point releases from Alma, Rocky, Oracle,
  Alpine and Debian (Debian's describe today's apt point release, which
  may be newer than the dated build). No osinfo ids (needs a decision on where the ids
  come from) and no firmware (no vendor states it, beyond Alpine's
  older `uefi` filenames).
- Vendor servers close idle keep-alive connections, and reusing one
  failed with "connection closed before message completed" (Fedora: 5
  of 24 runs). The generator's HTTP client keeps no idle connections
  (0 of 24 failed). A vendor whose run still fails transiently is tried
  3 times, 20 seconds apart, before it counts as failed.
- A single 404/410 removes a release; the two-consecutive-runs rule is
  not implemented, so a release served by some mirrors and not others
  (Fedora 42) flaps between runs.
- Debian lists `stable` and `oldstable` only. `testing` and `unstable`
  have no `Version` in their apt Release files, so they cannot be
  resolved, and there is no `dev` alias for Debian.
- Mapping problems still open:
  - FreeBSD lists point releases (15.1, 15.0, ...) as separate releases,
    unlike the release-is-the-major-version rule above; grouping them
    needs more than one build per release.
  - Fedora 42 is still in Fedora's feed but some mirrors no longer serve
    it (it is moving to `archives.fedoraproject.org`); it needs the
    vault handling above.

Resolver problems found on 2026-10-05 and fixed here: Alpine filenames
and EOL dates, openSUSE ordering and lifecycle (now from
`get.opensuse.org`), Fedora pre-release tokens, Debian exact dated
builds, and Ubuntu's fallback table. These fixes are not in the copies
in monitor-reef or the second copy.

## Prior art

- **osinfo-db / libosinfo** (GPL-2.0-or-later): per-OS-version XML used
  by virt-install, virt-manager and GNOME Boxes. Its `<image>` element
  has `arch`, `format`, `cloud-init`, optional `variant` and `url`; no
  checksums or build ids. Released roughly twice a year (v20250606,
  v20251212, v20260812). We complement it with fast-moving build data
  and reference its ids.
- **KubeVirt containerdisks** (`medius`, Go): discovers the latest
  images for six distros, parses vendor checksum files, and republishes
  them as container images on quay.io. Same discovery problem; a
  potential collaborator.
- **Canonical Simple Streams**: a static-file image feed format with
  hashes inline; a possible additional output format.
- **endoflife.date**: the model for "a directory, not a distributor".
- **redhatcloudx/cloud-image-directory-frontend**: an existing Red Hat
  repository by a similar name (last updated 2023-03-30); not yet
  investigated.

## Open questions

- Versioning beyond the `v1/` path prefix.
- GitHub scheduled workflows are disabled after about 60 days without
  repository activity in public repos; confirm whether bot commits
  count.
- Vendors without generic checksum files: Oracle (hashes only in an
  HTML table) and Talos (no hash; its image factory sends no hash or
  ETag headers as of 2026-10-05).
- Fedora: switch from `releases.json` to its CHECKSUM files (existence
  to be confirmed).
- This repository is MPL-2.0. Open: implications if osinfo-db data
  (GPL-2.0-or-later) is ever bundled rather than referenced.
- The custom domain long-lived clients will use, and whether clients
  must follow the redirect GitHub Pages issues from the `github.io`
  address once a custom domain is configured.
- Order of migration for `tritonadm` and the other Triton tooling to consume
  this repository's crates and index.
