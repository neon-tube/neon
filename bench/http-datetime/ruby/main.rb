# http-datetime: Ruby, standard library only (socket).
# TCPServer + a Thread per accepted connection, HTTP/1.1 keep-alive loop.
require "socket"

PORT = ENV.fetch("PORT", "18080").to_i

# Read one request off the socket: consume bytes through the "\r\n\r\n"
# header terminator. Returns the header text, or nil on EOF/error.
def read_request(sock)
  buf = +""
  while (idx = buf.index("\r\n\r\n")).nil?
    chunk = sock.readpartial(4096)
    buf << chunk
  end
  buf[0..idx + 3]
rescue EOFError, IOError, Errno::ECONNRESET
  buf.empty? ? nil : buf
end

def handle(sock)
  loop do
    headers = read_request(sock)
    break if headers.nil? || headers.empty?

    now = Time.now.utc.strftime("%Y-%m-%dT%H:%M:%SZ")
    keep_alive = headers !~ /^connection:\s*close/i

    resp = +"HTTP/1.1 200 OK\r\n"
    resp << "Content-Type: text/plain\r\n"
    resp << "Content-Length: #{now.bytesize}\r\n"
    resp << (keep_alive ? "Connection: keep-alive\r\n" : "Connection: close\r\n")
    resp << "\r\n"
    resp << now
    sock.write(resp)

    break unless keep_alive
  end
rescue Errno::EPIPE, Errno::ECONNRESET, IOError
  # A bad connection is dropped, never the whole server.
ensure
  sock.close rescue nil
end

server = TCPServer.new("127.0.0.1", PORT)
loop do
  client = server.accept
  Thread.new(client) { |c| handle(c) }
end
