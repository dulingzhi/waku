/**
 * Installs `crypto.getRandomValues` for Hermes.
 *
 * **This file exists to be imported first, and nothing else.** An ES module's
 * `import` declarations are all hoisted and evaluated before any other
 * statement in the importing module runs, so a polyfill written as a statement
 * beside an `import` runs *after* that import's module graph. Here the
 * installation is the module's own body: importing this file evaluates it, and
 * any module imported after it sees the polyfill already in place.
 *
 * Hermes has no `crypto`, and `@noble/curves` draws its X25519/Ed25519 keys
 * from `crypto.getRandomValues`, so the sealed handshake throws on the first
 * `randomSecretKey()` without this. That failure only shows on a device: the
 * Bun test suite runs on a runtime that already has WebCrypto.
 */
import { getRandomValues } from 'expo-crypto';

const globalCrypto = globalThis.crypto as Crypto | undefined;
if (typeof globalCrypto?.getRandomValues !== 'function') {
  Object.defineProperty(globalThis, 'crypto', {
    configurable: true,
    enumerable: true,
    value: { ...globalCrypto, getRandomValues },
    writable: true,
  });
}
