const assert = require('assert');
const test = require('node:test');
const fs = require('fs');

// Simple mock for browser globals before loading app.js
global.window = {
  __TAURI__: { core: { invoke: () => {} }, event: { listen: () => {} } },
  addEventListener: () => {}
};
global.document = {
  getElementById: () => ({ addEventListener: () => {} }),
  createElement: () => ({ setAttribute: () => {}, appendChild: () => {}, classList: { add: ()=>{}, remove: ()=>{} } }),
  createElementNS: () => ({ setAttribute: () => {} }),
  querySelectorAll: () => [],
  addEventListener: () => {},
  body: { classList: { add: () => {}, remove: () => {} } }
};
global.requestAnimationFrame = (cb) => setTimeout(cb, 0);

const code = fs.readFileSync(__dirname + '/app.js', 'utf8');

// Export pure functions for testing
const scriptToRun = code + `
;
({
  fmtBytes: typeof fmtBytes !== 'undefined' ? fmtBytes : null,
  shortHash: typeof shortHash !== 'undefined' ? shortHash : null,
  fileExt: typeof fileExt !== 'undefined' ? fileExt : null,
  folderName: typeof folderName !== 'undefined' ? folderName : null,
})
`;

const result = require('vm').runInThisContext(scriptToRun);

test('fmtBytes formats bytes correctly', () => {
  // Happy paths
  assert.strictEqual(result.fmtBytes(0), '0 B', '0 bytes');
  assert.strictEqual(result.fmtBytes(1023), '1023 B', 'less than 1 KB');
  assert.strictEqual(result.fmtBytes(1024), '1.00 KB', 'exactly 1 KB');
  assert.strictEqual(result.fmtBytes(1024 * 1024 * 2.5), '2.50 MB', 'fractional MB');
  assert.strictEqual(result.fmtBytes(1024 * 1024 * 1024 * 5.123), '5.12 GB', 'fractional GB');

  // Edge cases and error conditions
  assert.strictEqual(result.fmtBytes(null), '–', 'handles null');
  assert.strictEqual(result.fmtBytes(undefined), '–', 'handles undefined');
  assert.strictEqual(result.fmtBytes('1024'), '1.00 KB', 'handles string input');
});

test('shortHash truncates hash correctly', () => {
  // Happy paths
  assert.strictEqual(result.shortHash('1234567890abcdef1234567890abcdef'), '1234567890…cdef', 'truncates long hash');
  assert.strictEqual(result.shortHash('1234567890abc'), '1234567890…0abc', 'truncates exactly right length?');

  // Edge cases and error conditions
  assert.strictEqual(result.shortHash(''), '', 'handles empty string');
  assert.strictEqual(result.shortHash(null), '', 'handles null');
  assert.strictEqual(result.shortHash(undefined), '', 'handles undefined');
});

test('fileExt extracts file extension correctly', () => {
  // Happy paths
  assert.strictEqual(result.fileExt('photo.jpg'), 'jpg', 'simple extension');
  assert.strictEqual(result.fileExt('/path/to/archive.TAR.GZ'), 'gz', 'multiple extensions and path, lowercases result');
  assert.strictEqual(result.fileExt('noext'), '(no ext)', 'no extension');

  // Edge cases
  assert.strictEqual(result.fileExt('.hidden'), '(no ext)', 'hidden file without extension');
  assert.strictEqual(result.fileExt('.hidden.txt'), 'txt', 'hidden file with extension');
  assert.strictEqual(result.fileExt(''), '(no ext)', 'empty string');

  // Error conditions
  assert.throws(() => result.fileExt(null), TypeError, 'throws on null due to string method usage');
  assert.throws(() => result.fileExt(undefined), TypeError, 'throws on undefined');
});

test('folderName extracts folder name correctly', () => {
  // Happy paths
  assert.strictEqual(result.folderName('/a/b/c'), 'c', 'standard path');
  assert.strictEqual(result.folderName('/a/b/c/'), 'c', 'trailing slash');
  assert.strictEqual(result.folderName('/Volumes/My Drive/Photos'), 'Photos', 'spaces in path');
  assert.strictEqual(result.folderName('relative/path'), 'path', 'relative path');

  // Edge cases
  assert.strictEqual(result.folderName('/'), '/', 'root path');
  assert.strictEqual(result.folderName('name'), 'name', 'just a name');
  assert.strictEqual(result.folderName(''), '', 'empty string');

  // Error conditions
  assert.strictEqual(result.folderName(null), '', 'handles null gracefully');
  assert.strictEqual(result.folderName(undefined), '', 'handles undefined gracefully');
});
