# Mbed TLS used by Plum

This directory is a source snapshot of Mbed TLS 3.6.3, pinned to commit
`22098d41c6620ce07cf8a0134d37302355e1e5ef`.

It is kept in the repository so HTTPS builds do not depend on whichever TLS
library happens to be installed on a developer's machine. `bootstrap/mbedtls-build`
builds a private static archive from these sources. The self-hosted compiler
embeds the source and headers into generated user builds. The native shim uses
Unix CA bundle paths and the Windows system ROOT certificate store; a future
release can add a macOS Keychain-specific loader if `/etc/ssl/cert.pem` is
unavailable.
