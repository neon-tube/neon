-- http-datetime: Lua + luasocket (the de-facto standard socket library).
-- luasocket is single-threaded, so we multiplex: one coroutine per connection,
-- non-blocking sockets, and socket.select() as the readiness reactor. This gives
-- real concurrency across many keep-alive connections in a single OS thread.
local socket = require("socket")

local port = tonumber(os.getenv("PORT") or "18080")

local server = assert(socket.bind("127.0.0.1", port))
server:settimeout(0)

-- sock -> { co = <coroutine>, mode = "read"|"write" }
local waiting = {}

-- A coroutine yields ("read"|"write", sock) to say: wake me when sock is ready.
local function want(mode, sock)
  return coroutine.yield(mode, sock)
end

-- Receive one line, resuming across would-block boundaries. luasocket's "*l"
-- returns the partial read on timeout, which we feed back in as the prefix.
local function recv_line(client)
  local prefix = ""
  while true do
    local line, err, part = client:receive("*l", prefix)
    if line then return line end
    if err == "timeout" then
      prefix = part
      want("read", client)
    else
      return nil, err              -- closed / reset
    end
  end
end

-- Send everything, yielding when the socket buffer is full.
local function send_all(client, data)
  local i = 1
  while i <= #data do
    local sent, err, last = client:send(data, i)
    if sent then
      i = sent + 1
    elseif err == "timeout" then
      i = (last or (i - 1)) + 1
      want("write", client)
    else
      return false, err
    end
  end
  return true
end

-- Serve one connection: request/response, keep-alive until the peer hangs up.
local function serve(client)
  client:settimeout(0)
  while true do
    -- Request line. nil here means the client closed between requests: normal.
    local first = recv_line(client)
    if not first then return end

    local keep = true
    -- Consume header lines through the blank line that terminates the headers.
    while true do
      local line = recv_line(client)
      if not line then return end
      if line == "" then break end
      if line:lower():match("^connection:%s*close") then keep = false end
    end

    local now = os.date("!%Y-%m-%dT%H:%M:%SZ")   -- "!" => UTC
    local conn = keep and "keep-alive" or "close"
    local resp = "HTTP/1.1 200 OK\r\n"
      .. "Content-Type: text/plain\r\n"
      .. "Content-Length: " .. #now .. "\r\n"
      .. "Connection: " .. conn .. "\r\n\r\n"
      .. now
    if not send_all(client, resp) then return end
    if not keep then return end
  end
end

-- Resume a connection's coroutine one step; clean up when it finishes/errors.
local function step(co, sock, ...)
  local ok, mode = coroutine.resume(co, ...)
  if not ok or coroutine.status(co) == "dead" then
    waiting[sock] = nil
    pcall(function() sock:close() end)    -- drop bad/finished connection only
    return
  end
  waiting[sock] = { co = co, mode = mode }
end

-- Reactor loop.
while true do
  local recvt, sendt = { server }, {}
  for sock, w in pairs(waiting) do
    if w.mode == "read" then
      recvt[#recvt + 1] = sock
    else
      sendt[#sendt + 1] = sock
    end
  end

  local readable, writable = socket.select(recvt, sendt)

  for _, s in ipairs(readable) do
    if s == server then
      while true do
        local client = server:accept()
        if not client then break end
        step(coroutine.create(serve), client, client)   -- initial resume passes client
      end
    else
      local w = waiting[s]
      if w then step(w.co, s) end
    end
  end

  for _, s in ipairs(writable) do
    local w = waiting[s]
    if w and w.mode == "write" then step(w.co, s) end
  end
end
