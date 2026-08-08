# Serialization

`std::json` encodes and decodes, and it is the payload that finished protocol dispatch:
every dispatch feature built between 2026-07-22 and 2026-08-03 exists because some part of
this needed it. The module is small; what it cost was the machinery underneath.

## `ToJson`/`FromJson`, not `Serialize`/`Deserialize`

Serde's genericity comes from a `Serializer` type parameter threaded through every impl,
which buys one derive across many formats. Here it would buy nothing and cost a language
feature.

Nothing: Neon monomorphises, so there is no dictionary to share, and there is one format.

Cost: a `Serializer` protocol's methods must accept field values of *arbitrary* type, which
is a nested higher-order bound — and it lands exactly on the `Resolution::Bound` limit that
picks an impl by head string with no specificity ordering. The genericity would have had to
be built before the encoder could be.

The choice forecloses nothing. `impl[T] ToJson for T where T: Serialize` re-expresses this in
terms of a generic protocol later, on the bounded-impl machinery that already exists.

## `Json` is an ordinary union

```neon
mu type Json = null | bool | i64 | f64 | str | List[Json] | Map[str, Json]
```

Not an opaque handle. Every arm is a type the language already has, so `1.5`, `"hi"` and
`null` are `Json` values with no wrapping step, and a document is taken apart with `match`
rather than through an accessor API. Exhaustiveness checking applies: miss an arm and the
compiler names the one you missed.

**Numbers are `i64 | f64`**, against the format, which has one number type meaning a double.
A language with an `i64` that rendered `1` as `1.0`, or silently lost precision above 2^53,
would be lying about the value it was handed.

Decoding is deliberately asymmetric: an `i64` node *does* decode as an `f64`, because a
document written anywhere else spells `1.0` as `1`, and refusing it would fail every round
trip through another language on whole numbers. The reverse is not done — that direction
loses information rather than recovering it.

## The two derives are different tricks

`Display`'s generated body is an interpolated string; each field becomes a hole, and a hole
already dispatches `Display`. `ToJson`'s is one call to `std::json::object` with a list of
`(name, to_json(field))` pairs. Both leave the derive pass knowing nothing about field types:
a nested derived record, a `List[T]` with a library impl, and a primitive all encode through
whatever impl covers them.

`FromJson`'s is a third trick, and the reason return-position dispatch had to work first:

```neon
P { x: try from_json(try std::json::field(j, "x")), .. }
```

Nothing carrying the type is *passed*. `from_json` dispatches on the type its result is
checked against, and a record literal's field slot supplies exactly that.

**The generated body names `std::json::Json` and `std::json::object` absolutely**, while the
protocol keeps the author's spelling. The author chose the protocol and resolution must
settle it; nobody chose the scaffolding, and a generated body that compiled only when the
author happened to `use std::json` would break on someone else's file.

## What return-position dispatch cost

`fn from_json(j: Json) -> T` mentions the subject only in the return, so the expected type is
the dispatch subject. The checker had implemented that and nothing had ever exercised it.
Three defects fell out, none of which could touch `ToJson` — it dispatches on an argument and
returns a `Json` that does not mention the subject, so decode was the only thing that could
surface them:

- **`Resolution::Bound` read the receiver from `args[0]` regardless.** For a return-position
  call that is an unrelated argument. The visible half was a `<todo>` marker; the dangerous
  half is that an argument whose type happened to have an impl would have dispatched on it
  and silently run the wrong impl. The variant now carries `subject_pos`.
- **`protocol_method_result` returned the protocol's own subject variable.** Inside
  `impl[V] FromJson for Map[str, V]` the call's type came back as `T`, so it type-checked
  only because the `List[T]` impl next door happens to spell its parameter `T`. Two generic
  impls of one protocol was all it took.
- **Only the first `@derive` on a record was lowered.** Every generated impl carried the
  record's span, and `impl_def_at` matches by `(module, span)` first-wins, so the second
  impl's methods were indexed under the first's protocol and its body silently vanished.
  Unreachable while `Display` was the only derivable protocol.

## Union decode: dispatch refuses, and that is the answer

The plan was for the compiler to require the arms' JSON projections be pairwise disjoint.
**Dispatch cannot do that** — only the protocol knows which documents each impl accepts, so
"these two arms cannot both match" is a fact about JSON, not about dispatch, and a general
mechanism would be inventing semantics it cannot verify.

What was there instead was a segfault. A `Resolution::Switch` picks an arm by reading the
receiver's runtime tag, and a return-position dispatch has no receiver — so lowering switched
on the *document*, compared its tag against the subject's variants, and gave the last arm no
test at all. `dec(true)` on an `i64 | str` read a bool payload as a `neon_str`.

So dispatch refuses, and the diagnostic names the escape hatch: one impl for the whole union,
which inspects the value and decides. Selecting it needs one rule beyond ordinary
specificity — `most_specific` prefers the *narrower* heads, which is right when there is a
value to test and impossible when there is not, so in return position an impl covering the
whole subject wins instead.

A `@derive(FromJson)` for unions could generate that covering impl, and the disjointness
check belongs *there*, where the format is known.

## Object keys are sorted

A `Map` is unordered — `keys` walks the hash table, so the same map built two ways can walk
differently. Some order has to be imposed for the output to be a function of the value, and
sorted is the one stable across runs, builds and rehashes. Insertion order is not available
to recover, and field order would only be recoverable for derived records anyway.

## Escaping

`quote` escapes the two-character forms JSON names and uses `\u00XX` for every other C0
control. Bytes above 0x7F are **not** escaped: they are UTF-8 lead and continuation bytes,
and escaping them per byte would emit half a character. `string::byte_at` exists for this —
classifying a byte through a one-byte `str` costs a comparison per candidate, where `b < 32`
is one test.

## Pinned by

- `protocols/json_encodes_through_library_impls.neon` — the library impls, and the L5 repro
- `protocols/json_decodes_by_expected_type.neon` — return-position dispatch
- `protocols/return_dispatch_onto_a_union_needs_one_impl.neon` and its `_fails` companion
- `records/derive_to_json.neon`, `records/derive_from_json.neon`
- `protocols/library_impl_for_a_container.neon` — the litmus, now testing the stdlib's impls
