type op = 
  | Add of int 
  | Move of int 
  | Out 
  | In 
  | Loop of op list

let parse source =
  let len = String.length source in
  let rec parse_body pos acc =
    if pos >= len then (List.rev acc, pos)
    else
      match source.[pos] with
      | '+' | '-' as c ->
          let val_ = if c = '+' then 1 else -1 in
          let rec loop p v =
            if p < len && (source.[p] = '+' || source.[p] = '-') then
              loop (p + 1) (v + (if source.[p] = '+' then 1 else -1))
            else (p, v)
          in
          let (next_pos, final_val) = loop (pos + 1) val_ in
          if final_val <> 0 then parse_body next_pos (Add final_val :: acc)
          else parse_body next_pos acc
      | '>' | '<' as c ->
          let val_ = if c = '>' then 1 else -1 in
          let rec loop p v =
            if p < len && (source.[p] = '>' || source.[p] = '<') then
              loop (p + 1) (v + (if source.[p] = '>' then 1 else -1))
            else (p, v)
          in
          let (next_pos, final_val) = loop (pos + 1) val_ in
          if final_val <> 0 then parse_body next_pos (Move final_val :: acc)
          else parse_body next_pos acc
      | '.' -> parse_body (pos + 1) (Out :: acc)
      | ',' -> parse_body (pos + 1) (In :: acc)
      | '[' ->
          let (body, next_pos) = parse_body (pos + 1) [] in
          parse_body next_pos (Loop body :: acc)
      | ']' -> (List.rev acc, pos + 1)
      | _ -> parse_body (pos + 1) acc
  in
  let (ops, _) = parse_body 0 [] in ops

let execute ops =
  let tape = Array.make 30000 0 in
  let ptr = ref 0 in
  let rec exec ops =
    List.iter (function
      | Add v -> tape.(!ptr) <- tape.(!ptr) + v
      | Move v -> ptr := !ptr + v
      | Out -> Printf.printf "%d" tape.(!ptr)
      | In -> ()
      | Loop body -> while tape.(!ptr) <> 0 do exec body done
    ) ops
  in
  exec ops;
  tape.(8)

let () =
  let program = "++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>+<-]<-]<-]<-]<-]<-]<-]<-]" in
  let ops = parse program in
  let res = execute ops in
  Printf.printf "Result: %d\n" res
