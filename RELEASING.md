# Releasing aiusage

Builds, existing tests, package assembly, installation checks, and size reports
run in GitHub Actions. Do not build or pack releases locally.

## One-time prerequisites

1. Confirm upstream redistribution permission. Commit the applicable `LICENSE`
   (or `LICENSE.md` / `LICENSE.txt`) and notices, and set the correct package
   license metadata. Do not invent a license. Publication refuses unlicensed
   artifacts; dry-run packaging is allowed for validation.
2. Confirm ownership of the npm organization `@shanthropic`, publishing access
   to its packages, and the ability to claim the unscoped `aiusage` name.
3. Create the GitHub environment `npm`, restricted to tags `aiusage-v*`.
   Protect that tag namespace with a repository ruleset; optionally require
   release approval on the environment.
4. Run **Release packages** manually on `rust`. Download `release-tarballs`
   only after every package installation check passes. The artifact includes
   five native tarballs, the CLI tarball, and SHA-512 integrity/size metadata.
5. Bootstrap those six packages using interactive npm authentication with 2FA.
   Upload the five `shanthropic-aiusage-*.tgz` files first and `aiusage-*.tgz`
   last, using `npm publish <downloaded-file.tgz> --access public --ignore-scripts`.
   This uploads CI-built bytes; it does not compile or pack on your machine.
6. For **each of the six existing npm packages**, configure trusted publishing:
   GitHub owner `shanthropic`, repository `aiusage`, workflow `publish.yml`,
   environment `npm`, explicitly allowing `npm publish`.
   Configure trust near the next release: new registrations currently need a
   first successful publication within two days. After OIDC works, disable
   token publishing in npm package settings.

## Normal releases

Update `package.json` to a new stable `X.Y.Z` through a PR into `rust`.
Tag that merged commit `aiusage-vX.Y.Z` and push the tag. Do not move release
tags. Rust crate versions are independent; all six npm versions must match.

The tag workflow validates the tagged commit, builds five native targets,
stages exact-version optional dependencies, packs and checks six tarballs,
and checks installation on Node 22/24/26. Only the final publish job gets OIDC.
Native packages publish first; the CLI publishes only after their integrity is
verified. Post-publication npx checks verify real platform selection.

No version commits, branch pushes, release creation, or npm token are needed
in the publishing workflow. The public repository/public packages receive
provenance through npm trusted publishing.

## Failed/partial releases

- Rerun **failed jobs only**, retaining the original `release-tarballs` artifact.
  Do not rerun successful build/pack jobs after any package has published.
- Existing versions are skipped only if registry integrity matches the original
  tarball. Any mismatch stops publication. Network/authentication errors also
  stop the release rather than being interpreted as missing versions.
- Artifacts are retained for 30 days. If lost/expired, inspect what published
  and normally use a new version. Never rebuild replacement bytes under a
  partially published version, automatically unpublish, or move `latest` back.
- A failing post-publication npx check requires investigation and usually a fix
  release; npm publication cannot be rolled back atomically across six packages.

Tarball sizes and consumer install/cache sizes appear in workflow summaries.
Cache size includes registry metadata/overhead, not an exact network-byte meter.
The matching image-renderer binary is additional to the parser binary.
