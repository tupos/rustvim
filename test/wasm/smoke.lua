local api = vim.api

assert(vim.fn.strdisplaywidth('界é') == 3)
api.nvim_buf_set_lines(0, 0, -1, false, { 'hello 界é' })
assert(api.nvim_buf_get_lines(0, 0, -1, false)[1] == 'hello 界é')

local win = api.nvim_open_win(0, false, {
  relative = 'editor',
  row = 1,
  col = 2,
  width = 12,
  height = 3,
})
api.nvim_win_set_config(win, { relative = 'editor', row = 2, col = 3 })
api.nvim_win_close(win, true)

assert(not pcall(error, 'expected Lua error'))
assert(#api.nvim_get_runtime_file('parser/*', true) == 0)
local available = vim.treesitter.language._complete()
for _, lang in ipairs({
  'lua',
  'c',
  'diff',
  'vim',
  'vimdoc',
  'query',
  'markdown',
  'markdown_inline',
}) do
  assert(vim.list_contains(available, lang))
  assert(vim.treesitter.language.add(lang))
end
assert(vim.treesitter.language.add('lua_alias', { symbol_name = 'lua' }))
assert(not pcall(vim.treesitter.language.add, 'explicit_lua', {
  path = '/missing-parser.so',
  symbol_name = 'lua',
}))
local missing, message = vim.treesitter.language.add('rustvim_missing')
assert(missing == nil and message == 'No parser for language "rustvim_missing"')
local parser = vim.treesitter.get_string_parser('local x = 1', 'lua')
assert(parser:parse()[1]:root():type() == 'chunk')
assert(vim.fn.writefile({ 'ok' }, '/wasm-smoke-ok') == 0)
