type node = Node of node * node | Nil

let rec make d =
  if d = 0 then Node (Nil, Nil)
  else Node (make (d - 1), make (d - 1))

let rec check = function
  | Nil -> 0
  | Node (l, r) -> 1 + check l + check r

let () =
  let max_depth = 18 in
  let stretch = make (max_depth + 1) in
  let sc = check stretch in
  Printf.printf "stretch tree of depth %d check: %d\n" (max_depth + 1) sc;
  
  let long_lived = make max_depth in
  
  let total = ref sc in
  let depth = ref 4 in
  while !depth <= max_depth do
    let iterations = 1 lsl (max_depth - !depth + 4) in
    let sum = ref 0 in
    for _ = 1 to iterations do
      sum := !sum + check (make !depth)
    done;
    Printf.printf "%d trees of depth %d check: %d\n" iterations !depth !sum;
    total := !total + !sum;
    depth := !depth + 2
  done;
  
  let ll = check long_lived in
  Printf.printf "long lived tree of depth %d check: %d\n" max_depth ll;
  total := !total + ll;
  
  Printf.printf "Result: %d\n" !total
