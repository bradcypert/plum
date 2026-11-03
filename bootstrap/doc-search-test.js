// Run the generated scripts with a strict DOM double: interpreting any HTML
// fails. Check text nodes, navigation, ranking, result limits and cleanup.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
class Element {
  constructor(tag) { this.tag = tag; this.children = []; this.attrs = {}; this.listeners = {}; }
  set innerHTML(_) { throw new Error('Search must not interpret HTML'); }
  set textContent(value) { this.text = value; this.children = []; }
  setAttribute(key, value) { this.attrs[key] = value; }
  appendChild(child) { this.children.push(child); }
  replaceChildren() { this.children = []; }
  addEventListener(name, fn) { this.listeners[name] = fn; }
  blur() { this.blurred = true; }
  focus() { this.focused = true; }
}
const box = new Element('input'), out = new Element('ul');
const document = new Element('document');
document.getElementById = id => id === 'q' ? box : out;
document.createElement = tag => new Element(tag);
const context = vm.createContext({ document, window: {} });
for (const file of ['search-index.js', 'search.js']) {
  vm.runInContext(fs.readFileSync(path.join(process.argv[2], file), 'utf8'), context);
}
const rows = context.window.PLUM_DOCS;
box.value = 'find_me'; box.listeners.input();
assert.equal(out.hidden, false);
assert.equal(out.children.length, rows.filter(r => r.n === 'find_me').length);
for (let i = 0; i < out.children.length; i++) {
  const li = out.children[i], a = li.children[0];
  assert.equal(li.tag, 'li'); assert.equal(a.tag, 'a');
  const row = rows.filter(r => r.n === 'find_me')[i];
  assert.equal(a.attrs.href, row.u);
  assert.equal(a.children[0].text, row.n);
  assert.equal(a.children[1].text, row.m);
  const url = new URL(row.u, 'https://docs.test/reference/');
  assert.equal(url.origin, 'https://docs.test');
  assert(fs.existsSync(path.join(process.argv[2], decodeURIComponent(url.pathname.split('/').pop()))));
}
// The browser-side text boundary also holds for strings not expressible as
// Plum identifiers. A href is generated separately by the compiler.
context.window.PLUM_DOCS = Array.from({length: 55}, (_, i) => ({
  n: 'find_<img onerror=alert(1)>', m: '<svg onload=alert(1)>', s: '', u: 'ordinary.html#find_me'
}));
box.value = 'find_'; box.listeners.input();
assert.equal(out.children.length, 40);
assert.equal(out.children[0].children[0].children[0].text, 'find_<img onerror=alert(1)>');
// Prefix matches outrank substring matches even when they occur later;
// only the first forty in each rank can contribute to the display.
context.window.PLUM_DOCS = Array.from({length: 100}, (_, i) => ({
  n: i % 2 ? 'find_' + i : 'a_find_' + i, m: '', s: '', u: 'ordinary.html'
}));
box.listeners.input();
assert.equal(out.children.length, 40);
assert.equal(out.children[0].children[0].children[0].text, 'find_1');
assert.equal(out.children[39].children[0].children[0].text, 'find_79');
box.value = 'no such name'; box.listeners.input();
assert.equal(out.children.length, 1);
assert.equal(out.children[0].text, 'No match');
box.value = ''; box.listeners.input();
assert.equal(out.children.length, 0); assert.equal(out.hidden, true);
box.value = 'find'; box.listeners.input(); box.listeners.keydown({key:'Escape'});
assert.equal(out.children.length, 0); assert.equal(out.hidden, true); assert(box.blurred);
document.listeners.keydown({key:'/', preventDefault() {}}); assert(box.focused);
console.log('doc-search-test: generated search scripts render text and navigate safely');
