--- Neovim support for the Neon language.
---
--- Call `require('neon').setup{}` to start the language server for `.neon` buffers.
--- Filetype detection, syntax and indent work with no setup call at all -- they are
--- plain runtime files.
---
--- The language server (`neon-lsp`) advertises text sync, formatting, hover,
--- go-to-definition, references, rename, completion, signature help, document
--- symbols and inlay hints -- the `ServerCapabilities` literal near the top of
--- `lsp/src/main.rs` is the authoritative list. Keymaps for those are still opt-in
--- (`keymaps = true`), and inlay hints are still opt-in (`inlay_hints = true`),
--- for the reasons documented on `defaults` below. The old rule that this plugin
--- never binds a key for a capability the server lacks has not been relaxed; the
--- server simply grew the capabilities.

local M = {}

--- @class neon.Config
--- @field cmd? string[] The server command. Default: `{ 'neon-lsp' }`.
--- @field sysroot? string Value for `NEON_SYSROOT`. See `resolve_sysroot`.
--- @field root_markers? string[] Files that identify a project root.
--- @field settings? table Passed through to the server as `workspace/configuration`.
--- @field on_attach? fun(client: table, bufnr: integer)
--- @field capabilities? table Client capabilities, e.g. from a completion plugin.
--- @field format_on_save? boolean Default `false`.
--- @field autostart? boolean Default `true`.
--- @field warn_on_missing_sysroot? boolean Default `true`.
--- @field keymaps? boolean Bind the buffer-local LSP keymaps. Default `false`.
--- @field inlay_hints? boolean Turn inlay hints on at attach. Default `false`.
--- @field treesitter? boolean Register the parser with nvim-treesitter and start
--- it on `.neon` buffers when it is installed. Default `true`.

--- @type neon.Config
local defaults = {
  cmd = { 'neon-lsp' },
  sysroot = nil,
  -- `neon.toml` is the manifest a Neon project is rooted at (see cli/src/project.rs).
  -- `.git` is the fallback for a loose file inside a repository.
  root_markers = { 'neon.toml', '.git' },
  settings = {},
  format_on_save = false,
  autostart = true,
  warn_on_missing_sysroot = true,

  -- Off, and this is not timidity about whether the server answers -- it does.
  -- Neovim 0.11 already binds `K`, `grn`, `grr`, `gri` and `gra` globally for any
  -- attached client, so binding `gd`/`gr`/`<leader>rn` on top of that is adding a
  -- second, differently-spelled set of keys to a buffer that already had one. A
  -- plugin that silently rebinds `gd` in someone's config is a bug report, not a
  -- feature. Turn it on to get the older mnemonics; see `bind_keymaps`.
  keymaps = false,

  -- Off because inlay hints insert virtual text into the middle of lines. That is
  -- a change to how every file in the buffer *looks*, which is a preference, not a
  -- capability question. `:lua vim.lsp.inlay_hint.enable()` toggles at runtime.
  inlay_hints = false,

  -- On, because it costs nothing when the parser is absent: `start_treesitter`
  -- pcalls `vim.treesitter.start` and leaves `syntax/neon.vim` in charge when it
  -- fails. Nothing is installed or built by this flag.
  treesitter = true,
}

--- @type neon.Config
M.config = vim.deepcopy(defaults)

local function notify(msg, level)
  vim.notify('[neon] ' .. msg, level or vim.log.levels.INFO)
end

--- The sysroot to hand the server, or nil.
---
--- `neon-lsp` reads `NEON_SYSROOT` and expects a directory containing `stdlib/`. If
--- it is unset or wrong, the server does NOT fail -- `load_stdlib` returns an empty
--- list and the checker is skipped entirely, leaving only lexer and parser
--- diagnostics. That degradation is silent from the editor's side, which is exactly
--- the failure mode worth being loud about here.
---
--- Order of preference:
---   1. `config.sysroot`, as given.
---   2. `NEON_SYSROOT` already in the environment.
---   3. Nothing. The caller warns.
--- @return string|nil sysroot, string source
function M.resolve_sysroot()
  local configured = M.config.sysroot
  if type(configured) == 'function' then
    configured = configured()
  end
  if configured and configured ~= '' then
    return vim.fn.expand(configured), 'config.sysroot'
  end

  local env = vim.env.NEON_SYSROOT
  if env and env ~= '' then
    return env, 'NEON_SYSROOT'
  end

  return nil, 'unset'
end

--- Whether a resolved sysroot actually contains a `stdlib/` directory.
--- This mirrors what `load_stdlib` does: it joins `stdlib` onto the root and reads
--- it. A path without that subdirectory is as good as no path at all.
function M.sysroot_is_valid(root)
  return root ~= nil and vim.fn.isdirectory(root .. '/stdlib') == 1
end

local function project_root(bufnr)
  local name = vim.api.nvim_buf_get_name(bufnr)
  if name == '' then
    return vim.uv and vim.uv.cwd() or vim.loop.cwd()
  end
  local found = vim.fs.find(M.config.root_markers, {
    upward = true,
    path = vim.fs.dirname(name),
  })[1]
  if found then
    return vim.fs.dirname(found)
  end
  return vim.fs.dirname(name)
end

--- The environment the server is launched with: the editor's, plus NEON_SYSROOT.
local function server_env()
  local root = M.resolve_sysroot()
  if root then
    return { NEON_SYSROOT = root }
  end
  return nil
end

local warned = false

local function warn_sysroot_once()
  if warned or not M.config.warn_on_missing_sysroot then
    return
  end
  warned = true
  local root, source = M.resolve_sysroot()
  if root == nil then
    notify(
      'NEON_SYSROOT is not set. neon-lsp will report only lexer and parser errors -- '
        .. 'type errors will be missing entirely. Set it in your config '
        .. "(require('neon').setup{ sysroot = '/path/to/toolchain' }) or in your shell.",
      vim.log.levels.WARN
    )
  elseif not M.sysroot_is_valid(root) then
    notify(
      ('%s points at %q, which has no stdlib/ subdirectory. neon-lsp will skip type checking.')
        :format(source, root),
      vim.log.levels.WARN
    )
  end
end

--- Does this Neovim have the `vim.lsp.config` / `vim.lsp.enable` API?
---
--- Added in Neovim 0.11. Checked by feature rather than by version number, because
--- a version check would also have to be right about nightlies.
local function has_lsp_config_api()
  return type(vim.lsp) == 'table'
    and type(rawget(vim.lsp, 'config')) ~= 'nil'
    and type(rawget(vim.lsp, 'enable')) == 'function'
end

M.has_lsp_config_api = has_lsp_config_api

--- The bits of the config both code paths share.
local function base_config()
  return {
    cmd = M.config.cmd,
    filetypes = { 'neon' },
    settings = M.config.settings,
    capabilities = M.config.capabilities,
  }
end

--- Buffer-local keymaps for the capabilities the server actually advertises.
---
--- Every entry here is checked against `client.server_capabilities` first rather
--- than bound unconditionally. That is not defensive coding for its own sake: this
--- plugin is used against a `neon-lsp` built from a checkout, and a binary from
--- before the capability landed is a completely ordinary thing to have on `$PATH`.
--- A key that reports "no client supports this" is confusing; a key that was never
--- bound falls through to whatever the user had, which is the better failure.
local function bind_keymaps(client, bufnr)
  local function map(mode, lhs, rhs, cap, desc)
    if cap and not client.server_capabilities[cap] then
      return
    end
    vim.keymap.set(mode, lhs, rhs, { buffer = bufnr, desc = 'neon: ' .. desc })
  end

  map('n', 'K', vim.lsp.buf.hover, 'hoverProvider', 'hover')
  map('n', 'gd', vim.lsp.buf.definition, 'definitionProvider', 'go to definition')
  map('n', 'gr', vim.lsp.buf.references, 'referencesProvider', 'references')
  -- `renameProvider` refuses a symbol defined outside the current file and returns
  -- an LSP error rather than a partial edit. That surfaces as an error message,
  -- which is the correct outcome -- a rename that silently missed the definition
  -- would be worse than one that declined.
  map('n', '<leader>rn', vim.lsp.buf.rename, 'renameProvider', 'rename')
  map('n', '<leader>ca', vim.lsp.buf.code_action, 'codeActionProvider', 'code action')
  map('n', '<leader>f', function()
    vim.lsp.buf.format({ bufnr = bufnr, id = client.id, timeout_ms = 3000 })
  end, 'documentFormattingProvider', 'format')
  -- Insert mode as well as normal: signature help is wanted mid-call, and the
  -- server's trigger characters (`(` and `,`) only fire on a completion-capable
  -- client, so an explicit key is the reliable path.
  map({ 'n', 'i' }, '<C-s>', vim.lsp.buf.signature_help, 'signatureHelpProvider', 'signature help')
  map('n', '<leader>ds', vim.lsp.buf.document_symbol, 'documentSymbolProvider', 'document symbols')
  -- No capability gate: diagnostics are a notification the server pushes, not
  -- something it advertises in `ServerCapabilities`, so there is nothing to check.
  map('n', '<leader>e', vim.diagnostic.open_float, nil, 'line diagnostics')
  map('n', '<leader>q', vim.diagnostic.setloclist, nil, 'diagnostics to loclist')
end

local function attach(client, bufnr)
  if M.config.keymaps then
    bind_keymaps(client, bufnr)
  end

  -- `vim.lsp.inlay_hint` arrived in 0.10. On anything older the capability is
  -- advertised by the server and simply cannot be consumed here.
  if M.config.inlay_hints and client.server_capabilities.inlayHintProvider then
    if vim.lsp.inlay_hint and type(vim.lsp.inlay_hint.enable) == 'function' then
      vim.lsp.inlay_hint.enable(true, { bufnr = bufnr })
    else
      notify(
        'inlay_hints = true, but this Neovim has no vim.lsp.inlay_hint.enable (needs 0.10+).',
        vim.log.levels.WARN
      )
    end
  end

  if M.config.format_on_save then
    local group = vim.api.nvim_create_augroup('NeonFormatOnSave' .. bufnr, { clear = true })
    vim.api.nvim_create_autocmd('BufWritePre', {
      group = group,
      buffer = bufnr,
      desc = 'neon: format with neon-lsp before writing',
      callback = function()
        -- A file that does not parse yields no edits at all (the server returns an
        -- empty list rather than an error), so this is safe mid-edit.
        vim.lsp.buf.format({ bufnr = bufnr, id = client.id, timeout_ms = 3000 })
      end,
    })
  end
  if M.config.on_attach then
    M.config.on_attach(client, bufnr)
  end
end

--- Start the server for a buffer, on Neovim versions without `vim.lsp.enable`.
--- `vim.lsp.start` has existed since 0.8.
local function start_legacy(bufnr)
  local cfg = base_config()
  cfg.name = 'neon-lsp'
  cfg.root_dir = project_root(bufnr)
  cfg.cmd_env = server_env()
  cfg.on_attach = attach
  cfg.filetypes = nil -- not a `vim.lsp.start` field
  vim.lsp.start(cfg, { bufnr = bufnr })
end

--- Tell nvim-treesitter where the Neon grammar lives, so `:TSInstall neon` works.
---
--- The grammar is `extra/tree-sitter-neon` in this same repository, not a package on
--- the tree-sitter organisation, so nvim-treesitter cannot know about it: the
--- `install_info` table below is the only way it learns the url, the subdirectory and
--- the fact that `scanner.c` must be compiled in. That last one is not optional --
--- Neon's block comments nest, no regular expression can count, and the nesting depth
--- lives in the external scanner. A parser built from `parser.c` alone links, loads,
--- and then mis-parses every nested block comment.
---
--- This registers; it does not install. `:TSInstall neon` still has to be run, and it
--- needs a C compiler and network access.
local function register_parser()
  local ok, parsers = pcall(require, 'nvim-treesitter.parsers')
  if not ok then
    return false
  end
  -- nvim-treesitter's `main` branch dropped `get_parser_configs` in favour of a
  -- plain table of specs. Support whichever this checkout has rather than picking
  -- one and breaking on the other.
  local configs = type(parsers.get_parser_configs) == 'function' and parsers.get_parser_configs()
    or parsers
  if type(configs) ~= 'table' then
    return false
  end
  if configs.neon == nil then
    configs.neon = {
      install_info = {
        url = 'https://github.com/jkbbwr/neon',
        location = 'extra/tree-sitter-neon',
        files = { 'src/parser.c', 'src/scanner.c' },
      },
      filetype = 'neon',
    }
  end
  return true
end

--- Start tree-sitter highlighting on a buffer, if a `neon` parser is installed.
---
--- Returns true only when the parser loaded. On false the buffer keeps
--- `syntax/neon.vim`, which is why that file is still here and still maintained: a
--- parser has to be compiled, and "no highlighting until you run :TSInstall" is not
--- an acceptable out-of-the-box state.
---
--- `vim.treesitter.start` is wrapped in `pcall` because it throws when the parser is
--- absent -- an expected condition here, not an error worth propagating.
---
--- The two failures are told apart on purpose, because only one of them is silent
--- for a good reason:
---
---   * No parser installed. Ordinary, and the fallback is fine. Say nothing.
---   * A parser called `neon` loads, but `highlights.scm` does not apply to it. That
---     means the installed parser is a *different Neon grammar* -- the predecessor
---     repository (`jkbbwr/neon`) shipped one whose node names are incompatible, and
---     an installed copy of it is a completely ordinary thing to have. Observed
---     here: an old `neon.so` on the runtimepath fails with `Invalid node type
---     "doc_comment"`. Falling back quietly from that is indistinguishable from
---     having no parser at all, and the fix -- reinstall the parser -- is not
---     something a user guesses. So it is reported, once.
local ts_warned = false

local function start_treesitter(bufnr)
  if not M.config.treesitter then
    return false
  end
  local ok, err = pcall(vim.treesitter.start, bufnr, 'neon')
  if not ok then
    local loaded = pcall(vim.treesitter.language.add, 'neon')
    if loaded and not ts_warned then
      ts_warned = true
      notify(
        'a `neon` tree-sitter parser is installed but this plugin\'s queries do not fit it '
          .. '-- most likely the grammar from the predecessor repository, whose node names are '
          .. 'incompatible. Reinstall with `:TSUninstall neon` then `:TSInstall neon`. '
          .. 'Falling back to syntax/neon.vim.\n'
          .. tostring(err),
        vim.log.levels.WARN
      )
    end
    return false
  end
  -- Both highlighters running at once means the regex syntax paints over the
  -- tree-sitter result in places, and the disagreements are subtle rather than
  -- obvious. `vim.treesitter.start` already sets `b:ts_highlight`; clearing
  -- `syntax` is what actually stops the vim one.
  vim.bo[bufnr].syntax = ''
  return true
end

M.start_treesitter = start_treesitter

--- Configure and enable the language server.
--- @param opts neon.Config|nil
function M.setup(opts)
  M.config = vim.tbl_deep_extend('force', vim.deepcopy(defaults), opts or {})

  if M.config.treesitter then
    register_parser()
    vim.api.nvim_create_autocmd('FileType', {
      pattern = 'neon',
      group = vim.api.nvim_create_augroup('NeonTreesitter', { clear = true }),
      desc = 'neon: start tree-sitter highlighting when the parser is installed',
      callback = function(args)
        start_treesitter(args.buf)
      end,
    })
    for _, buf in ipairs(vim.api.nvim_list_bufs()) do
      if vim.api.nvim_buf_is_loaded(buf) and vim.bo[buf].filetype == 'neon' then
        start_treesitter(buf)
      end
    end
  end

  -- Everything above this line is editor-side and works with no server at all.
  -- The early return below is only about the LSP half.
  if vim.fn.executable(M.config.cmd[1]) == 0 then
    notify(
      ('%q is not on $PATH. Build it with `cargo build --release -p neon-lsp` and either '
        .. 'add it to $PATH or set `cmd` to its absolute path.'):format(M.config.cmd[1]),
      vim.log.levels.WARN
    )
    return
  end

  if not M.config.autostart then
    return
  end

  if has_lsp_config_api() then
    -- Neovim 0.11+. `vim.lsp.enable` attaches on FileType for the configured
    -- filetypes; no autocmd of ours is involved.
    local cfg = base_config()
    cfg.cmd_env = server_env()
    cfg.root_markers = M.config.root_markers
    cfg.on_attach = attach
    vim.lsp.config['neon-lsp'] = cfg
    vim.lsp.enable('neon-lsp')
    vim.api.nvim_create_autocmd('FileType', {
      pattern = 'neon',
      group = vim.api.nvim_create_augroup('NeonLsp', { clear = true }),
      desc = 'neon: sysroot sanity check',
      callback = warn_sysroot_once,
    })
    -- `setup` may run after a .neon buffer is already open, in which case the
    -- FileType event for it has been and gone.
    for _, buf in ipairs(vim.api.nvim_list_bufs()) do
      if vim.api.nvim_buf_is_loaded(buf) and vim.bo[buf].filetype == 'neon' then
        warn_sysroot_once()
        break
      end
    end
  else
    -- Neovim 0.8 - 0.10.
    vim.api.nvim_create_autocmd('FileType', {
      pattern = 'neon',
      group = vim.api.nvim_create_augroup('NeonLsp', { clear = true }),
      desc = 'neon: start neon-lsp',
      callback = function(args)
        warn_sysroot_once()
        start_legacy(args.buf)
      end,
    })
    -- `setup` may be called after the first .neon buffer already exists.
    for _, buf in ipairs(vim.api.nvim_list_bufs()) do
      if vim.api.nvim_buf_is_loaded(buf) and vim.bo[buf].filetype == 'neon' then
        warn_sysroot_once()
        start_legacy(buf)
      end
    end
  end
end

--- What tree-sitter is actually doing for Neon right now.
---
--- "A parser is installed" is deliberately not the question asked. It was, and it
--- gave the wrong answer on this machine: an incompatible `neon.so` from the
--- predecessor grammar loads perfectly well through `vim.treesitter.language.add`
--- and then fails the moment a query is run against it. So the check is whether
--- `highlights.scm` compiles against the loaded language -- the thing that has to
--- work for highlighting to happen.
local function ts_status()
  if not M.config.treesitter then
    return 'disabled in config (syntax/neon.vim in use)'
  end
  if not pcall(vim.treesitter.language.add, 'neon') then
    return 'no parser installed (syntax/neon.vim in use; :TSInstall neon)'
  end
  local ok, err = pcall(vim.treesitter.query.get, 'neon', 'highlights')
  if not ok then
    return ('parser installed but queries do not fit it -- reinstall it (%s)'):format(err)
  end
  return 'parser found, queries compile'
end

--- Print what the plugin resolved. For "why is this not working".
function M.info()
  local root, source = M.resolve_sysroot()
  local lines = {
    'neon.nvim',
    ('  command:      %s'):format(table.concat(M.config.cmd, ' ')),
    ('  executable:   %s'):format(
      vim.fn.executable(M.config.cmd[1]) == 1 and vim.fn.exepath(M.config.cmd[1]) or 'NOT FOUND'
    ),
    ('  sysroot:      %s (from %s)'):format(root or 'unset', source),
    ('  stdlib/ ok:   %s'):format(tostring(M.sysroot_is_valid(root))),
    ('  lsp api:      %s'):format(has_lsp_config_api() and 'vim.lsp.enable (0.11+)' or 'vim.lsp.start'),
    ('  treesitter:   %s'):format(ts_status()),
    ('  keymaps:      %s'):format(M.config.keymaps and 'on' or 'off'),
    ('  inlay hints:  %s'):format(M.config.inlay_hints and 'on' or 'off'),
  }

  -- The capability list is read off the live client rather than printed from a
  -- constant. A constant is exactly what goes stale: this line used to read
  -- "diagnostics, formatting (that is all the server advertises)" and stayed that
  -- way through eight capabilities being added to `lsp/src/main.rs`.
  local clients = vim.lsp.get_clients and vim.lsp.get_clients({ name = 'neon-lsp' })
    or vim.lsp.get_active_clients({ name = 'neon-lsp' })
  if clients[1] then
    local caps = {}
    for name, value in pairs(clients[1].server_capabilities or {}) do
      if value ~= false and value ~= nil then
        caps[#caps + 1] = name
      end
    end
    table.sort(caps)
    lines[#lines + 1] = ('  capabilities: %s'):format(table.concat(caps, ', '))
  else
    -- Not necessarily "no server": `initialize` is a round trip, so running
    -- :NeonInfo in the same tick that opened the buffer lands here too.
    lines[#lines + 1] =
      '  capabilities: (no neon-lsp client attached yet -- open a .neon file, or give the handshake a moment)'
  end

  notify(table.concat(lines, '\n'))
end

return M
