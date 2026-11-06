-- Markdown preview, nvim side. The GUI runs this on every attached server and
-- passes its channel id. Everything goes out as 'nvright_md' notifications:
--   open  <bool>
--   text  {path, distro, lines, hl}   buffer contents (debounced) + code block highlights
--   view  {topline, botline, lines}
--   theme {fg, bg, heading, ...}
local chan = ...
local api = vim.api

local M = { open = false, buf = nil, dirty = false, timer = nil, attached = {} }
_G.nvright_preview = M

-- Text goes out at most every DEBOUNCE ms, after the key has been handled,
-- so the keystroke itself never pays for it.
local DEBOUNCE = 30

local function notify(kind, data)
  pcall(vim.rpcnotify, chan, 'nvright_md', kind, data)
end

local function is_markdown(buf)
  return api.nvim_buf_is_valid(buf) and vim.bo[buf].filetype == 'markdown'
end

-- fg + bold/italic of a group, walking treesitter's dotted fallback
-- ('@keyword.function.lua' -> '@keyword.function' -> '@keyword').
local hl_cache = {}
local function group_style(name)
  local cached = hl_cache[name]
  if cached ~= nil then
    return cached
  end
  local style = false
  local probe = name
  while probe do
    local ok, hl = pcall(api.nvim_get_hl, 0, { name = probe, link = false })
    if ok and hl.fg then
      style = { hl.fg, (hl.bold and 1 or 0) + (hl.italic and 2 or 0) }
      break
    end
    probe = probe:match('^(.+)%.[^.]+$')
  end
  hl_cache[name] = style
  return style
end

-- fence info ('js', 'py', ...) -> installed parser name, or false
local lang_cache = {}
local function fence_lang(info)
  local cached = lang_cache[info]
  if cached ~= nil then
    return cached
  end
  local lang = false
  local candidates = { vim.treesitter.language.get_lang(info) or info }
  local ft = vim.filetype.match({ filename = 'x.' .. info })
  if ft then
    candidates[#candidates + 1] = vim.treesitter.language.get_lang(ft) or ft
  end
  for _, name in ipairs(candidates) do
    if pcall(vim.treesitter.language.add, name) and pcall(vim.treesitter.query.get, name, 'highlights') then
      lang = name
      break
    end
  end
  lang_cache[info] = lang
  return lang
end

-- Each block gets its own string parser instead of reusing the buffer's tree,
-- since nvim's highlighter only parses what's on screen. Rows are relative to
-- the block; output is flat (row0, col0, row1, col1, color, flags).
local function block_highlights(lang, text)
  local out = {}
  local ok, parser = pcall(vim.treesitter.get_string_parser, text, lang)
  if not ok then
    return out
  end
  local trees = parser:parse()
  local query = vim.treesitter.query.get(lang, 'highlights')
  if not (trees and trees[1] and query) then
    return out
  end
  for id, node in query:iter_captures(trees[1]:root(), text, 0, -1) do
    local capture = query.captures[id]
    if capture:sub(1, 1) ~= '_' and capture ~= 'spell' and capture ~= 'nospell' then
      local style = group_style('@' .. capture .. '.' .. lang)
      if style then
        local r0, c0, r1, c1 = node:range()
        local n = #out
        out[n + 1], out[n + 2], out[n + 3], out[n + 4] = r0, c0, r1, c1
        out[n + 5], out[n + 6] = style[1], style[2]
      end
    end
  end
  return out
end

-- All fenced blocks, in buffer coordinates. Cached by lang + content, so
-- typing outside a code block is free.
local block_cache = {}
local function code_highlights(lines)
  local out = {}
  local cache = {}
  local i, n = 1, #lines
  while i <= n do
    local indent, fence, info = lines[i]:match('^( ? ? ?)(```+)%s*([%w_+#.-]*)')
    if not fence then
      indent, fence, info = lines[i]:match('^( ? ? ?)(~~~+)%s*([%w_+#.-]*)')
    end
    if fence then
      local close = '^' .. indent:gsub(' ', ' ?') .. fence:sub(1, 1):rep(#fence) .. '+%s*$'
      local j = i + 1
      while j <= n and not lines[j]:match(close) do
        j = j + 1
      end
      local lang = info ~= '' and fence_lang(info:lower())
      if lang and j > i + 1 then
        local text = table.concat(lines, '\n', i + 1, j - 1)
        local key = lang .. '\0' .. text
        local hl = block_cache[key] or block_highlights(lang, text)
        cache[key] = hl
        for k = 1, #hl, 6 do
          local m = #out
          out[m + 1], out[m + 2] = hl[k] + i, hl[k + 1]
          out[m + 3], out[m + 4] = hl[k + 2] + i, hl[k + 3]
          out[m + 5], out[m + 6] = hl[k + 4], hl[k + 5]
        end
      end
      i = j + 1
    else
      i = i + 1
    end
  end
  block_cache = cache
  return out
end

local function send_text()
  M.dirty = false
  local buf = M.buf
  if not (M.open and buf and api.nvim_buf_is_valid(buf)) then
    return
  end
  local lines = api.nvim_buf_get_lines(buf, 0, -1, false)
  local ok, hl = pcall(code_highlights, lines)
  notify('text', {
    path = api.nvim_buf_get_name(buf),
    distro = vim.env.WSL_DISTRO_NAME or '',
    lines = lines,
    hl = ok and hl or {},
  })
end

-- current window if it shows the buffer, else the first one in the tab
local function tracked_window()
  local win = api.nvim_get_current_win()
  if api.nvim_win_get_buf(win) == M.buf then
    return win
  end
  for _, w in ipairs(api.nvim_tabpage_list_wins(0)) do
    if api.nvim_win_get_buf(w) == M.buf then
      return w
    end
  end
end

local function send_view()
  if not (M.open and M.buf and api.nvim_buf_is_valid(M.buf)) then
    return
  end
  local win = tracked_window()
  if not win then
    return
  end
  local info = vim.fn.getwininfo(win)[1]
  notify('view', {
    topline = info.topline,
    botline = info.botline,
    lines = api.nvim_buf_line_count(M.buf),
  })
end

local function schedule_text()
  M.dirty = true
  M.timer = M.timer or vim.uv.new_timer()
  M.timer:stop()
  M.timer:start(DEBOUNCE, 0, vim.schedule_wrap(function()
    if M.dirty then
      send_text()
      send_view()
    end
  end))
end

local function color(names, attr)
  for _, name in ipairs(names) do
    local ok, hl = pcall(api.nvim_get_hl, 0, { name = name, link = false })
    if ok and hl[attr] then
      return hl[attr]
    end
  end
end

local function send_theme()
  hl_cache = {}
  block_cache = {}
  local function heading(n)
    return color({ '@markup.heading.' .. n .. '.markdown', '@markup.heading.' .. n }, 'fg')
  end
  notify('theme', {
    fg = color({ 'Normal' }, 'fg'),
    bg = color({ 'Normal' }, 'bg'),
    heading = color({ '@markup.heading.1.markdown', '@markup.heading', 'Title' }, 'fg'),
    heading1 = heading(1),
    heading2 = heading(2),
    heading3 = heading(3),
    heading4 = heading(4),
    heading5 = heading(5),
    heading6 = heading(6),
    note = color({ 'DiagnosticInfo' }, 'fg'),
    tip = color({ 'DiagnosticOk', 'DiagnosticHint' }, 'fg'),
    important = color({ 'DiagnosticHint', 'Special' }, 'fg'),
    warning = color({ 'DiagnosticWarn', 'WarningMsg' }, 'fg'),
    caution = color({ 'DiagnosticError', 'ErrorMsg' }, 'fg'),
    link = color({ '@markup.link.url', '@markup.link', 'Underlined', 'Identifier' }, 'fg'),
    code = color({ '@markup.raw.markdown_inline', '@markup.raw', 'String' }, 'fg'),
    code_bg = color({ 'CursorLine', 'ColorColumn', 'Pmenu' }, 'bg'),
    quote = color({ '@markup.quote', 'Comment' }, 'fg'),
    muted = color({ 'Comment', 'NonText' }, 'fg'),
    rule = color({ 'WinSeparator', 'VertSplit', 'NonText' }, 'fg'),
  })
end

local function track(buf)
  M.buf = buf
  if not M.attached[buf] then
    M.attached[buf] = true
    api.nvim_buf_attach(buf, false, {
      on_lines = function(_, b)
        if not M.open then
          M.attached[b] = nil
          return true -- detach
        end
        if b == M.buf then
          schedule_text()
        end
      end,
      on_reload = function(_, b)
        if b == M.buf then
          schedule_text()
        end
      end,
      on_detach = function(_, b)
        M.attached[b] = nil
      end,
    })
  end
  send_text()
  send_view()
end

function M.toggle(on)
  if on == nil then
    on = not M.open
  end
  M.open = on
  notify('open', on)
  if not on then
    return
  end
  send_theme()
  local buf = api.nvim_get_current_buf()
  if is_markdown(buf) then
    track(buf)
  elseif M.buf and api.nvim_buf_is_valid(M.buf) then
    track(M.buf)
  end
end

-- click in the preview: jump to that line (1-based)
function M.goto_line(line)
  local win = M.buf and api.nvim_buf_is_valid(M.buf) and tracked_window()
  if not win then
    return
  end
  api.nvim_set_current_win(win)
  line = math.max(1, math.min(line, api.nvim_buf_line_count(M.buf)))
  api.nvim_win_set_cursor(win, { line, 0 })
end

-- Link to another file: open it in the markdown file's window, relative to
-- its directory. Only if it exists.
function M.open_file(target)
  local win = M.buf and api.nvim_buf_is_valid(M.buf) and tracked_window()
  if not win then
    return
  end
  local path = target
  if not vim.startswith(path, '/') then
    local dir = vim.fs.dirname(api.nvim_buf_get_name(M.buf))
    path = vim.fs.normalize(dir .. '/' .. target)
  end
  if not vim.uv.fs_stat(path) then
    vim.notify('nvright: file not found: ' .. path, vim.log.levels.WARN)
    return
  end
  api.nvim_set_current_win(win)
  vim.cmd.edit(vim.fn.fnameescape(path))
end

local group = api.nvim_create_augroup('nvright_preview', { clear = true })
-- Switching to a non-markdown buffer keeps the last markdown file in the
-- panel, otherwise the grid would get resized on every buffer switch.
api.nvim_create_autocmd({ 'BufEnter', 'FileType' }, {
  group = group,
  callback = function(ev)
    if M.open and ev.buf == api.nvim_get_current_buf() and is_markdown(ev.buf) and ev.buf ~= M.buf then
      track(ev.buf)
    end
  end,
})
api.nvim_create_autocmd({ 'WinScrolled', 'WinEnter', 'BufWinEnter' }, {
  group = group,
  callback = function()
    if M.open then
      send_view()
    end
  end,
})
api.nvim_create_autocmd('ColorScheme', {
  group = group,
  callback = function()
    if M.open then
      send_theme()
    end
  end,
})
api.nvim_create_user_command('NvrightPreview', function(opts)
  M.toggle(opts.bang and false or nil)
end, { bang = true, desc = 'nvright: markdown preview (! closes it)' })
