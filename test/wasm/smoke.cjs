const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const stderr = [];

// Load the packaged editor and execute its Lua checks inside WebAssembly.
async function main() {
  const modulePath = path.resolve(process.argv[2]);
  const createNvim = require(modulePath);
  const editor = await createNvim({
    noInitialRun: true,
    locateFile: (file) => path.join(path.dirname(modulePath), file),
    printErr: (message) => stderr.push(message),
    preRun: [(module) => {
      module.ENV.HOME = '/home/user';
      module.ENV.VIMRUNTIME = '/runtime';
      module.FS.mkdirTree('/home/user');
      module.FS.writeFile('/wasm-smoke.lua', fs.readFileSync(path.join(__dirname, 'smoke.lua')));
    }],
  });
  assert(editor.HEAPU8.buffer instanceof SharedArrayBuffer);

  const args = ['nvim', '--headless', '-u', 'NONE', '-i', 'NONE', '-n', '-l', '/wasm-smoke.lua'];
  const pointers = args.map((text) => {
    const length = editor.lengthBytesUTF8(text) + 1;
    const pointer = editor._malloc(length);
    assert.notEqual(pointer, 0);
    editor.stringToUTF8(text, pointer, length);
    return pointer;
  });
  const argv = editor._malloc((pointers.length + 1) * 4);
  assert.notEqual(argv, 0);
  pointers.forEach((pointer, index) => editor.setValue(argv + index * 4, pointer, '*'));
  editor.setValue(argv + pointers.length * 4, 0, '*');

  try {
    editor._nvim_main(pointers.length, argv);
  } catch (error) {
    if (error?.status !== 0) throw error;
  }
  assert.equal(editor.FS.readFile('/wasm-smoke-ok', { encoding: 'utf8' }), 'ok\n');
  console.log('WASM shared memory, Unicode editing, floating windows, Lua errors, and parsers passed.');
}

main().catch((error) => {
  for (const message of stderr) console.error(message);
  console.error(error);
  process.exitCode = 1;
});
