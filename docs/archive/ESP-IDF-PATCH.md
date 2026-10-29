# Local ESP-IDF driver changes

Base: Espressif ESP-IDF 5.5.4, Git commit `735507283d5b2f9fb363a1901172dbd9e847945d`.

The local checkout contained changes to HTTP client transport, parallel LCD I/O and USB request definitions. These changes are preserved in [esp-idf-v5.5.4-local.patch](esp-idf-v5.5.4-local.patch).

Restore a clean ESP-IDF checkout at the recorded commit, initialize its submodules, then apply the patch from the SDK root:

```sh
git checkout 735507283d5b2f9fb363a1901172dbd9e847945d
git submodule update --init --recursive
git apply /path/to/esp-claw/docs/archive/esp-idf-v5.5.4-local.patch
```

The patch was checked against clean copies of all four base files; applying it reproduced the local modified files byte-for-byte. A new firmware cross-build or device test was not performed during archival.
