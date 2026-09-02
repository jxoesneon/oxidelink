# extract-zip-patched

Vendored fork of [`extract-zip@2.0.1`](https://www.npmjs.com/package/extract-zip)
with a security patch for **CVE-2026-56876 / GHSA-jmr9-qjv8-65gv**.

## Why this exists

The upstream `extract-zip` package (<= 2.0.1) does not validate symlink
targets when extracting zip archives. A malicious zip containing a symlink
with a relative path like `../../../../etc/passwd` would be extracted without
validation, allowing the symlink to point outside the extraction directory.

No patched version has been published upstream. This vendored copy adds the
missing validation.

## The patch

In `extractEntry()`, before creating a symlink, the resolved target path is
checked against the extraction root:

```js
const linkTarget = path.resolve(path.dirname(dest), link)
const relativeTarget = path.relative(this.opts.dir, linkTarget)
if (relativeTarget.startsWith('..') || path.isAbsolute(link)) {
  throw new Error(
    `Refusing to create symlink "${entry.fileName}" -> "${link}": ` +
    `target resolves outside the extraction directory (CVE-2026-56876)`
  )
}
```

This rejects any symlink whose target escapes the extraction directory or
uses an absolute path.

## How it's used

The `e2e-tests/package.json` overrides `extract-zip` to point at this local
directory via `"file:vendor/extract-zip-patched"`. npm replaces all transitive
copies of `extract-zip` with this patched version.

## License

BSD-2-Clause (inherited from upstream `extract-zip`).
