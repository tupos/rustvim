local t = require('test.testutil')
local n = require('test.functional.testnvim')()
local tt = require('test.functional.testterm')

local describe, it, after_each, pending = t.describe, t.it, t.after_each, t.pending
local assert_log = t.assert_log
local assert_nolog = t.assert_nolog
local clear = n.clear
local command = n.command
local eq = t.eq
local exec_lua = n.exec_lua
local expect_exit = n.expect_exit
local request = n.request
local matches = t.matches
local read_file = t.read_file

describe('log', function()
  local testlog = 'Xtest_logging'

  after_each(function()
    expect_exit(command, 'qa!')
    vim.uv.sleep(10) -- Wait for Nvim to fully exit
    os.remove(testlog)
    os.remove(testlog .. '.old')
  end)

  it('skipped before log_init', function()
    -- This test is for _visibility_: adjust as needed, after checking for regression.
    --
    -- During startup some components may try to log before logging is setup.
    -- That should be uncommon (ideally never)--and if there are MANY such
    -- calls, that needs investigation.
    clear()
    eq(0, request('nvim__stats').log_skip)
    clear { env = { CDPATH = '~doesnotexist' } }
    assert(request('nvim__stats').log_skip <= 13)
  end)

  it('TUI client name is "ui"', function()
    local function setup(env)
      clear()
      -- Start Nvim with builtin UI.
      local screen = tt.setup_child_nvim({
        '-u',
        'NONE',
        '-i',
        'NONE',
        '--cmd',
        n.nvim_set,
      }, {
        env = env,
      })
      screen:expect([[
        ^                                                  |
        ~                                                 |*4
                                                          |
        {5:-- TERMINAL --}                                    |
      ]])
    end

    -- Without $NVIM parent.
    setup({
      NVIM = '',
      NVIM_LISTEN_ADDRESS = '',
      NVIM_LOG_FILE = testlog,
      __NVIM_TEST_LOG = '1',
    })
    -- Example:
    --    ERR 2024-09-11T16:40:02.421 ui.47056   ui_client_run:165: test log message
    assert_log(' ui%.%d+% +ui_client_run:%d+: test log message', testlog, 100)

    -- With $NVIM parent.
    setup({
      NVIM_LOG_FILE = testlog,
      __NVIM_TEST_LOG = '1',
    })
    -- Example:
    --    ERR 2024-09-11T16:41:17.539 ui/c/T2.47826.0 ui_client_run:165: test log message
    local tid = _G._nvim_test_id
    assert_log(' ui/c/' .. tid .. '%.%d+%.%d +ui_client_run:%d+: test log message', testlog, 100)
  end)

  it('formats messages with session name or test id', function()
    -- Examples:
    --    ERR 2024-09-11T16:44:33.794 T3.49429.0 server_init:58: test log message
    --    ERR 2024-09-11T16:44:33.823 c/T3.49429.0 server_init:58: test log message

    clear({
      env = {
        NVIM_LOG_FILE = testlog,
        -- TODO: remove this after nvim_log #7062 is merged.
        __NVIM_TEST_LOG = '1',
      },
    })

    local tid = _G._nvim_test_id
    assert_log(tid .. '%.%d+%.%d +server_init:%d+: test log message', testlog, 100)

    exec_lua([[
      local j1 = vim.fn.jobstart({ vim.v.progpath, '-es', '-V1', '+foochild', '+qa!' }, vim.empty_dict())
      vim.fn.jobwait({ j1 }, 5000)
    ]])

    -- Child Nvim spawned by jobstart() prepends "c/" to parent name.
    assert_log('c/' .. tid .. '%.%d+%.%d +server_init:%d+: test log message', testlog, 100)
  end)

  it('warns when $NVIM_LOG_FILE is inaccessible', function()
    clear({
      args_rm = { '-u' },
      args = { '--clean' },
      env = { NVIM_LOG_FILE = '/foo/bar' },
    })
    t.retry(nil, nil, function()
      t.matches('log: "/foo/bar" not accessible, logging to', n.exec_capture('messages'))
    end)

    clear({
      args_rm = { '-u' },
      args = { '--clean' },
      env = { NVIM_LOG_FILE = '/foo/bar', XDG_STATE_HOME = '/foo2/bar2' },
    })
    t.retry(nil, nil, function()
      t.matches('log: "/foo/bar" not accessible, logging to', n.exec_capture('messages'))
    end)
  end)

  ---Starts a fresh native logger and declares only its existing public C ABI.
  ---@param env? table<string, string>
  local function setup_native(env)
    clear({ env = vim.tbl_extend('force', { NVIM_LOG_FILE = testlog }, env or {}) })
    if not exec_lua("return pcall(require, 'ffi')") then
      pending('N/A: missing LuaJIT FFI')
    end
    exec_lua([[
      require('ffi').cdef([=[
        bool logmsg(int level, const char *context, const char *function_name,
                    int line, bool newline, const char *format, ...);
        void log_callstack(const char *function_name, int line);
        void log_uv_handles(void *loop);
        int log_set_level(int level);
        extern unsigned char main_loop[1];
      ]=])
    ]])
  end

  it('writes the complete native header and printf arguments through Rust', function()
    setup_native()
    eq(true, exec_lua(function()
      local ffi = require('ffi')
      return ffi.C.logmsg(
        3, 'ctx: ', 'lua_log_test', 123, true,
        'value=%d text=%s', ffi.new('int', 42), 'héllo'
      )
    end))
    local timestamp = '%d%d%d%d%-%d%d%-%d%dT%d%d:%d%d:%d%d%.%d%d%d'
    matches(
      '^WRN ' .. timestamp .. ' [^\n]+ ctx: lua_log_test:123: value=42 text=héllo\n$',
      read_file(testlog)
    )
  end)

  it('uses the current Rust-owned severity threshold', function()
    setup_native()
    eq({ false, false, true, true }, exec_lua(function()
      local ffi = require('ffi')
      local saved = ffi.C.log_set_level(3)
      local emitted = {
        ffi.C.logmsg(1, nil, 'level_test', 1, true, 'skip-debug'),
        ffi.C.logmsg(2, nil, 'level_test', 2, true, 'skip-info'),
        ffi.C.logmsg(3, nil, 'level_test', 3, true, 'keep-warning'),
        ffi.C.logmsg(4, nil, 'level_test', 4, true, 'keep-error'),
      }
      ffi.C.log_set_level(saved)
      return emitted
    end))
    assert_nolog('skip%-debug', testlog)
    assert_nolog('skip%-info', testlog)
    assert_log('WRN .-level_test:3: keep%-warning', testlog)
    assert_log('ERR .-level_test:4: keep%-error', testlog)
  end)

  it('restores severity after nested Lua scopes and callback errors', function()
    setup_native()
    eq({ false, { true, false, true, false, true }, false }, exec_lua(function()
      local ffi = require('ffi')
      local saved = ffi.C.log_set_level(3)
      ---Returns whether INFO passes the current scoped threshold.
      local function info()
        return ffi.C.logmsg(2, nil, 'scope_test', 1, true, 'scoped-info')
      end

      local before = info()
      local scoped = vim._with({ log_level = 2 }, function()
        local outer = info()
        local inner = vim._with({ log_level = 4 }, info)
        local restored = info()
        local ok = pcall(vim._with, { log_level = 5 }, function()
          error('expected callback failure')
        end)
        return { outer, inner, restored, ok, info() }
      end)
      local after = info()
      ffi.C.log_set_level(saved)
      return { before, scoped, after }
    end))
  end)

  it('preserves missing source metadata and the newline flag', function()
    setup_native()
    eq(true, exec_lua(function()
      return require('ffi').C.logmsg(3, nil, nil, -1, false, 'no-newline')
    end))
    matches(' %?:no%-newline$', read_file(testlog))
    eq(false, read_file(testlog):sub(-1) == '\n')
  end)

  it('retries long printf messages and accepts a following message', function()
    setup_native()
    eq({ true, true }, exec_lua(function()
      local ffi = require('ffi')
      return {
        ffi.C.logmsg(3, nil, 'long_test', 1, true, '%s %d',
                     string.rep('é', 4096), ffi.new('int', 42)),
        ffi.C.logmsg(3, nil, 'long_test', 2, true, 'after-long'),
      }
    end))
    local contents = read_file(testlog)
    eq(true, contents:find(string.rep('é', 4096) .. ' 42\n', 1, true) ~= nil)
    matches('long_test:2: after%-long\n$', contents)
  end)

  it('replaces invalid native UTF-8 in context, source, and payload', function()
    setup_native()
    eq(true, exec_lua(function()
      local ffi = require('ffi')
      local invalid = string.char(255)
      return ffi.C.logmsg(3, 'ctx' .. invalid .. ': ', 'fn' .. invalid, 9, true,
                          '%s', 'a' .. invalid .. 'b')
    end))
    local contents = read_file(testlog)
    matches(' ctx�: fn�:9: a�b\n$', contents)
    eq(nil, contents:find(string.char(255), 1, true))
  end)

  it('reopens the destination after log rotation', function()
    setup_native()
    eq(true, exec_lua(function()
      return require('ffi').C.logmsg(3, nil, 'rotation_test', 1, true, 'before-rotate')
    end))
    assert(os.rename(testlog, testlog .. '.old'))
    eq(true, exec_lua(function()
      return require('ffi').C.logmsg(3, nil, 'rotation_test', 2, true, 'after-rotate')
    end))
    matches('rotation_test:1: before%-rotate', read_file(testlog .. '.old'))
    matches('rotation_test:2: after%-rotate', read_file(testlog))
    assert_nolog('before%-rotate', testlog)
  end)

  it('captures a C caller with external symbolizers unavailable', function()
    setup_native({ PATH = '', RUST_BACKTRACE = '0', RUST_LIB_BACKTRACE = '0' })
    if not exec_lua(function()
      local ffi = require('ffi')
      local ok = pcall(function()
        return ffi.C.log_callstack
      end)
      return ok
    end) then
      pending('N/A: native stack logging is unavailable')
    end
    exec_lua(function()
      local ffi = require('ffi')
      ffi.cdef([[
        typedef struct MultiQueue MultiQueue;
        typedef void (*TestPutCallback)(MultiQueue *, void *);
        typedef struct {
          void (*handler)(void **);
          void *argv[10];
        } TestLogEvent;
        MultiQueue *multiqueue_new(TestPutCallback on_put, void *data);
        void multiqueue_put_event(MultiQueue *queue, TestLogEvent event);
        void multiqueue_process_events(MultiQueue *queue);
        void multiqueue_free(MultiQueue *queue);
      ]])
      local callback = ffi.cast('void (*)(void **)', function()
        ffi.C.log_callstack('lua_stack_test', 77)
      end)
      local event = ffi.new('TestLogEvent')
      event.handler = callback
      local queue = ffi.C.multiqueue_new(nil, nil)
      ffi.C.multiqueue_put_event(queue, event)
      ffi.C.multiqueue_process_events(queue)
      ffi.C.multiqueue_free(queue)
      callback:free()
    end)
    local contents = read_file(testlog)
    matches('lua_stack_test:77: trace:', contents)
    matches('multiqueue_process_events', contents)
    matches('multiqueue%.c:%d+', contents)
  end)

  it('writes libuv handles through the native diagnostic adapter', function()
    setup_native()
    exec_lua(function()
      local ffi = require('ffi')
      -- uv_loop_t is the first member of Neovim's struct loop.
      ffi.C.log_uv_handles(ffi.C.main_loop)
    end)
    matches('%[[R%-][A%-][I%-]%] +async +0x%x+', read_file(testlog))
  end)
end)
