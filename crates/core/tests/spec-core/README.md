# Vendored WebAssembly Spec Test Suite

These `.wast` files are vendored from the [WebAssembly spec repository](https://github.com/WebAssembly/spec) at commit `072bd0dc`.

**Updating the corpus:**
1. Check out the desired commit from the upstream spec repo
2. Copy the updated `test/core/*.wast` files into this directory
3. Review the diff to ensure only expected changes
4. Update the pin recorded in this README

The spec profile targeted is MVP + bulk-memory + sign-extension + threads.
