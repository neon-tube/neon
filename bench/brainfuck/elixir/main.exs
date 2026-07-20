defmodule Brainfuck do
  def parse(source) do
    chars = String.graphemes(source)
    {ops, _} = parse_body(chars)
    ops
  end

  defp parse_body(chars) do
    parse_acc(chars, [])
  end

  defp parse_acc([], acc), do: {Enum.reverse(acc), []}
  defp parse_acc([c | rest] = chars, acc) do
    case c do
      c when c in ["+", "-"] ->
        {val, remaining} = parse_rle(chars, 0, ["+", "-"])
        if val != 0 do
          parse_acc(remaining, [{:add, val} | acc])
        else
          parse_acc(remaining, acc)
        end
      c when c in [">", "<"] ->
        {val, remaining} = parse_rle(chars, 0, [">", "<"])
        if val != 0 do
          parse_acc(remaining, [{:move, val} | acc])
        else
          parse_acc(remaining, acc)
        end
      "." -> parse_acc(rest, [{:out} | acc])
      "," -> parse_acc(rest, [{:in} | acc])
      "[" ->
        {body, remaining} = parse_body(rest)
        parse_acc(remaining, [{:loop, body} | acc])
      "]" ->
        {Enum.reverse(acc), rest}
      _ ->
        parse_acc(rest, acc)
    end
  end

  defp parse_rle([], val, _chars), do: {val, []}
  defp parse_rle([c | rest] = current, val, allowed) do
    if c in allowed do
      inc = if c in ["+", ">"], do: 1, else: -1
      parse_rle(rest, val + inc, allowed)
    else
      {val, current}
    end
  end

  def execute(ops, tape, ptr) do
    Enum.reduce(ops, {tape, ptr}, fn op, {t, p} ->
      case op do
        {:add, val} ->
          curr = :array.get(p, t)
          {:array.set(p, curr + val, t), p}
        {:move, val} ->
          {t, p + val}
        {:out} ->
          IO.write(to_string(:array.get(p, t)))
          {t, p}
        {:in} ->
          {t, p}
        {:loop, body} ->
          loop_execute(body, t, p)
      end
    end)
  end

  defp loop_execute(body, tape, ptr) do
    if :array.get(ptr, tape) != 0 do
      {t2, p2} = execute(body, tape, ptr)
      loop_execute(body, t2, p2)
    else
      {tape, ptr}
    end
  end

  def main do
    program = "++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>+<-]<-]<-]<-]<-]<-]<-]<-]"
    ops = parse(program)
    tape = :array.new(30000, default: 0)
    {final_tape, _ptr} = execute(ops, tape, 0)
    val = :array.get(8, final_tape)
    IO.puts("Result: #{val}")
  end
end

Brainfuck.main()
