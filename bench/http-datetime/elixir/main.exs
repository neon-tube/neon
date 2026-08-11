# http-datetime: Elixir, Erlang :gen_tcp only.
# A spawned process per accepted socket, HTTP/1.1 keep-alive recv loop.
defmodule HttpDateTime do
  def start do
    port = System.get_env("PORT", "18080") |> String.to_integer()

    {:ok, listen} =
      :gen_tcp.listen(port, [
        :binary,
        ip: {127, 0, 0, 1},
        active: false,
        reuseaddr: true,
        backlog: 1024,
        packet: :raw
      ])

    accept_loop(listen)
  end

  defp accept_loop(listen) do
    case :gen_tcp.accept(listen) do
      {:ok, socket} ->
        # Hand the socket to a fresh process, but make it wait for :go so it
        # never touches the socket before ownership has actually transferred.
        pid = spawn(fn -> receive do: (:go -> serve(socket, "")) end)
        :gen_tcp.controlling_process(socket, pid)
        send(pid, :go)
        accept_loop(listen)

      {:error, _} ->
        accept_loop(listen)
    end
  end

  # Serve one connection: read a request, reply, keep the socket open.
  defp serve(socket, buf) do
    case read_request(socket, buf) do
      {:ok, headers, rest} ->
        now = DateTime.utc_now() |> DateTime.truncate(:second) |> DateTime.to_iso8601()
        keep = not Regex.match?(~r/^connection:\s*close/im, headers)
        conn = if keep, do: "keep-alive", else: "close"

        resp = [
          "HTTP/1.1 200 OK\r\n",
          "Content-Type: text/plain\r\n",
          "Content-Length: ",
          Integer.to_string(byte_size(now)),
          "\r\n",
          "Connection: ",
          conn,
          "\r\n\r\n",
          now
        ]

        case :gen_tcp.send(socket, resp) do
          :ok when keep -> serve(socket, rest)
          _ -> :gen_tcp.close(socket)
        end

      :closed ->
        :gen_tcp.close(socket)
    end
  end

  # Accumulate bytes until the "\r\n\r\n" header terminator is present.
  defp read_request(socket, buf) do
    case :binary.match(buf, "\r\n\r\n") do
      {pos, 4} ->
        headers = binary_part(buf, 0, pos + 4)
        rest = binary_part(buf, pos + 4, byte_size(buf) - pos - 4)
        {:ok, headers, rest}

      :nomatch ->
        case :gen_tcp.recv(socket, 0) do
          {:ok, data} -> read_request(socket, buf <> data)
          {:error, _} -> :closed
        end
    end
  end
end

HttpDateTime.start()
Process.sleep(:infinity)
