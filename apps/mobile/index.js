/**
 * The app entry.
 *
 * Two imports, and their order is the only thing this file does:
 *
 * 1. `./crypto-polyfill` installs `crypto.getRandomValues` as a side effect.
 * 2. `expo-router/entry` registers the root component.
 *
 * Both `import` declarations are hoisted to the top of the module, but module
 * graphs are evaluated **in source order**, so the polyfill's body runs before
 * the router — and therefore before any route module that reaches
 * `@noble/curves` at import time.
 *
 * What does not work, and was tried first: writing the polyfill as statements
 * between two imports. Those statements run after *both* imports have been
 * evaluated, so the router would load with no `crypto` and the first key
 * generation on a real device would throw. Verified: an ES module's own
 * statements run after its entire import graph.
 */
import './crypto-polyfill';

import 'expo-router/entry';
