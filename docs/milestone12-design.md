# Milestone 12 design: AVG, MIN, MAX, and SELECT-list aliases

Implemented on `feat/milestone-12`. The file format was not bumped. Two measurements differed from the estimate in this document: `Value` is 32 bytes because the ratio variant carries an `i128`, and `AccValue` is 48 bytes. A unique explicit alias wins `ORDER BY` even when another select item has the same text as its implicit name. That is the precedence example (`SELECT department AS name, name ... ORDER BY name`), which would have been ambiguous if every output name were treated the same.

The engine today stores four cell kinds: `NULL`, `INTEGER` (`i64`), `TEXT`, and `BOOLEAN` (`src/value.rs`). The heap codec writes them with tags 0–3 (`src/storage/record.rs`). `COUNT` and `SUM` share a 16-byte `Copy` accumulator (`AccValue` in `src/executor.rs`). A row's aggregates are applied to a staged copy and copied back only when every aggregate in that row succeeds. `SELECT` column aliases are rejected in `convert_projection` with `column aliases are not supported`. Table aliases stay as they are. SQL has no arithmetic, and floating-point literals are already rejected.

## 1. Architecture overview

Milestone 12 adds three aggregates and names for select-list items. It does not add outer joins, a third table, `DISTINCT` aggregates, window functions, `HAVING` alias resolution, arithmetic, or a new on-disk type.

```text
SELECT list
  parser: AVG | MIN | MAX, and `AS` / implicit alias
  binder: argument type, shared accumulator slot, output name
        |
        v
scan or two-table INNER JOIN (unchanged visitors)
  one input row at a time
        |
        v
GroupSink
  stage every aggregate of this row
  commit the staged slots only if all of them succeed
        |
        v
materialize group row
  keys, then COUNT / SUM / AVG / MIN / MAX values
        |
        v
HAVING (no alias lookup) -> ORDER BY (alias lookup) -> LIMIT -> project
```

`AVG` is accepted only on `INTEGER`. Its running state is a checked `i128` sum and an `i64` count. The value that leaves the accumulator is a reduced ratio, not an `i64` and not an `f64`. That ratio exists in query results, `ORDER BY`, and `HAVING`. It is not a column type and the record codec refuses to write it.

`MIN` and `MAX` accept `INTEGER`, `TEXT`, and `BOOLEAN`. They use the same total order as `compare_values`: `i64` order, Rust `String` order, and `false` then `true`. They ignore `NULL` and yield `NULL` when the group has no non-null input. The result cell keeps the argument's runtime type.

A select-list alias is a name for an output column. The accumulator, the group key, and the join visitor continue to see the underlying column or aggregate. `ORDER BY` may name that alias. `WHERE`, `ON`, `GROUP BY`, and `HAVING` do not.

`EXPLAIN` still type-checks and does not execute. Its aggregate text stays the rendered call (`AVG(salary)`), including a call that appears only in `HAVING` or `ORDER BY`. An alias does not add an `EXPLAIN` column.

## 2. AVG numeric representation decision

### Recommendation

Use approach C for the accumulator and approach B for the query result, implemented inside the tree with no new crate:

- While scanning, store `sum: i128` and `count: i64` on the `AVG` slot.
- When the group is materialized, reduce `sum / count` to a query-only ratio.
- Compare and filter on that ratio. Do not divide in `f64`, and do not return the integer quotient.

Approach A, a floating-point result, is rejected. An `f64` cannot represent every `i64` (the exact integer range stops at 2^53). `AVG` of a single value `9007199254740993` would be wrong, and ordering two close averages could disagree with the exact quotient. That violates "return fractional averages correctly" on the integer domain this engine already stores.

Approach B alone, a decimal or rational crate, is rejected as a dependency. The project depends on `fs4` and `sqlparser` only. A fixed-scale decimal also has to round `1/3`, which reintroduces the truncation this milestone is not allowed to hide. A rational is the right result type; it does not need to be the running accumulator, and it does not need a third-party type.

Approach C alone is incomplete. A wider accumulator still has to become a result. Returning `sum / count` as `i64` truncates `1` and `2` to `0`. Returning the wide sum without a denominator is not an average. The fractional result has to be stored separately from the persisted `INTEGER` type. That result is the reduced ratio.

### Running state

| Input | Slot after the row |
| --- | --- |
| `NULL` | unchanged |
| `i64` value | `sum = sum.checked_add(value as i128)`, `count = count.checked_add(1)` |
| no non-null input yet | `sum = 0`, `count = 0`, including a scalar query over zero rows |

`i64::MIN` is sign-extended to `i128` before the add, so its magnitude fits. Each accepted input is one exact add. There is no per-row division and no rounding.

`count` uses the same checked `i64` increment as `COUNT`. Failure is `AVG overflows INTEGER`. The `i128` sum fails with `AVG overflows`. Either error aborts the statement. Both are checked so a later change in row width cannot wrap.

An `i128` sum of `i64` values overflows only past 2^64 additions of a large magnitude. The check stays anyway. A group that overflows does not publish a wrapped average.

### Result

Materialize only when `count > 0`. Reduce by `gcd`, force the denominator to be positive, and put the sign on the numerator. `0` becomes `0/1`. A single-row average of `n` becomes `n/1`. The average of `1` and `2` becomes `1/2`.

The query cell is a new `Value` variant:

```rust
Value::Ratio { numerator: i128, denominator: i64 }
```

Every `AVG` result has a denominator greater than zero. The constructor also accepts a negative denominator and reduces it onto the numerator when the reduced form fits. This variant is not a `DataType`. `CREATE TABLE` cannot declare it. The parser still rejects floating-point literals, and it does not grow a ratio literal. Only `AVG` constructs the variant during a query.

`data_type()` stays `None` for `NULL` and also returns `None` for a ratio, because a ratio is not a column type. Expression typing must not treat that `None` as an untyped null. `HAVING` and comparison use a separate scalar type (section 7) whose `Ratio` case is distinct from null.

### Display

`Display` for `Value`, which `src/format.rs` already uses, prints a ratio as follows:

- Terminating base-10 fractions print the shortest exact decimal. `1/2` prints `0.5`. `3/2` prints `1.5`. A whole value prints with one decimal place so a ratio is visible in the REPL: `4/1` prints `4.0`.
- A fraction that does not terminate in base 10 prints the reduced ratio: `1/3` prints `1/3`, `-5/2` is terminating and prints `-2.5`.

`ORDER BY` and `HAVING` never parse that text back. They compare the reduced numerator and denominator. Display rounding is not a second numeric mode, because non-terminating values are not rounded.

### Comparison

`HAVING AVG(salary) > 100` promotes the integer literal to `100/1`. `AVG` compared with `TEXT` or `BOOLEAN` is a type error of the same shape as today's mixed-type comparison. Ordering operators are legal on ratios. Logical operators still require `BOOLEAN`.

Compare `a/b` and `c/d` by the sign of `(a*d - c*b) / (b*d)`, without converting to `f64`:

1. When both denominators are positive and both cross products fit in `i128`, compare those products.
2. Otherwise form the products in 256 bits. Reverse the product order when `b*d` is negative.

The first sketch of step 2 used Euclidean quotients. That fallback is not what shipped. `i128::MIN.div_euclid(-1)` overflows, and a remainder test that multiplies by a negative denominator flips the order. The 256-bit product covers those inputs and every positive-denominator `AVG` result. A zero denominator is rejected at construction, comparison, and display. It is not treated as zero. `AVG` never builds one: the count is checked before `Value::ratio`.

`AVG` builds only a positive denominator in `1..=i64::MAX`. Reduction puts the sign on the numerator. Two reduced forms do not fit in that pair: a positive numerator of `2^127`, and a denominator of `2^63`. The constructor keeps the original bits for those two, and display and comparison still use the exact value. Neither is reachable from `AVG`.

A terminating expansion has at most 63 digits because the absolute denominator is at most `2^63`. The formatter stops at 64 digits and prints the reduced fraction if a remainder remains, so a malformed value cannot loop or allocate without a bound. Integral values still print with `.0`.

Negative averages stay exact. `AVG` of `-3` and `-2` is `-5/2`, which compares less than `-2/1`.

### Argument checks

The binder accepts `AVG` only when the argument column is `INTEGER`. `AVG(name)` and `AVG(active)` fail at bind time with `AVG requires INTEGER, not TEXT` or `not BOOLEAN`, including on an empty table and under `LIMIT 0`. `AVG(*)` is rejected by the parser (`AVG requires an INTEGER column`). Expressions, `DISTINCT`, `FILTER`, and nested calls stay on the existing rejection paths.

`NULL` inputs are skipped. A scalar query with no rows, and a group whose integer inputs are all `NULL`, materialize `NULL`. They do not divide by zero and they do not return `0.0`.

### What was rejected about storing the ratio

The ratio is not written to a heap record, a catalog entry, or an index key. Section 6 is the fence. Promoting `i64` columns to a decimal column type would be a format change and is out of scope.

## 3. MIN/MAX type and comparison semantics

`MIN` and `MAX` take one column. The parser rejects `MIN(*)`, `MAX(*)`, `DISTINCT`, extra arguments, and non-column expressions with the existing aggregate errors (`MIN requires a column` for the star form, and the current messages for the rest).

| Argument | Result type | Order |
| --- | --- | --- |
| `INTEGER` | `INTEGER` | `i64` order, so `i64::MIN` is the smallest integer |
| `TEXT` | `TEXT` | `String` order, the same order `compare_values` uses for `ORDER BY` |
| `BOOLEAN` | `BOOLEAN` | `false` then `true`, the order documented in the README |

The first non-null input initializes the extreme. A later value replaces it only when `compare_values` says it is strictly smaller (`MIN`) or strictly greater (`MAX`). An equal value keeps the value already stored, so a tie does not allocate another `String`. `NULL` never initializes and never replaces. Zero non-null inputs, including a scalar over zero rows, produce `NULL`.

`TEXT` order is byte order of UTF-8, case sensitive, with the empty string below every non-empty string. It is not a locale order. That is already the documented `ORDER BY` behavior, and `MIN`/`MAX` must not invent a second one.

### Boolean ordering

`check_binary` rejects `<`, `>`, `<=`, and `>=` on `BOOLEAN` (`operator '>' is not supported for BOOLEAN`). Equality and `IS NULL` are allowed. `ORDER BY` does not use `check_binary`; it uses `compare_values`, which already orders `false` before `true`.

`MIN` and `MAX` follow `compare_values`, not the `WHERE` operator restriction. `MIN(active)` and `MAX(active)` are valid. `WHERE active > FALSE` stays rejected. `HAVING MIN(active) > FALSE` stays rejected for the same reason. `HAVING MIN(active) = FALSE` is valid and uses boolean equality.

This split is deliberate. The aggregate needs the total order the sorter already has. The milestone does not relax boolean predicates.

### Types and sharing

The result `Value` is `Integer`, `Text`, or `Boolean`, never a ratio. The expression type of the aggregate is the argument column's type, so `HAVING` can compare `MIN(age)` with an integer and `MIN(name)` with text. A null result is still that expression type at bind time and `NULL` at runtime, matching `SUM`.

Two calls share one slot when the function and the bound column ordinal match. `MIN(name)` and `MIN(employees.name)` share. `MIN(name)` and `MAX(name)` do not. `MIN(salary)` and `AVG(salary)` do not. `EXPLAIN` still de-duplicates labels by rendered text, so two different spellings of the same call can appear twice in the `aggregates` text while occupying one slot. That is the Milestone 10 rule and it stays.

## 4. Alias resolution rules

### Syntax

`convert_projection` today returns `column aliases are not supported` for `ExprWithAlias` and `ExprWithAliases`. Milestone 12 accepts a single alias:

- `SELECT department AS dept`
- `SELECT department dept`

Both are `ExprWithAlias` in sqlparser. The alias is one identifier, normalized like every other identifier: unquoted names are folded to lowercase, quoted names keep their case. An empty alias is `column alias must not be empty`. `ExprWithAliases` (more than one alias) stays unsupported. `SELECT * AS anything`, wildcard `EXCLUDE` / `RENAME`, and table-column aliases on `FROM` stay unsupported.

The alias is stored beside the existing select item. The item's expression is unchanged: a column is still a `SelectColumn`, an aggregate is still an `AggregateCall`. Execution, grouping, and accumulator identity use that expression.

When the user writes no alias, the output name is the name used today: the schema column name for a column (`scope.name_at`, not the qualified spelling), and `AggregateCall::render` for an aggregate (`COUNT(*)`, `AVG(salary)`). That default name is the item's implicit alias for `ORDER BY`.

### Output

The result header uses the alias when one was written, and the implicit name otherwise. `SELECT department AS dept, COUNT(*) AS total` reports columns `dept` and `total`.

### ORDER BY

An `ORDER BY` item is still a column reference or an aggregate call. A qualified name (`employees.total`, `u.name`) is never an alias. It resolves with today's scope rules.

An unqualified name resolves in this order:

1. If exactly one select item has that output name, bind to that output ordinal. The ordinal is the group-key slot or the aggregate slot already chosen for that item. This does not allocate a second accumulator.
2. If two or more select items have that output name, fail with `column 'total' is ambiguous`.
3. If no select item has that name, resolve it as a source column, including the current ambiguity and "not in GROUP BY" checks.

A name that is both an output alias and a source column uses the alias when it is unqualified. `SELECT department AS name, name FROM employees ORDER BY name` orders by `department`. `ORDER BY employees.name` orders by the source column. This is the precedence rule.

`ORDER BY total DESC` where `total` is `COUNT(*) AS total` sorts the existing `COUNT(*)` slot. `ORDER BY COUNT(*)` on the same query shares that slot too. `ORDER BY` of an aggregate expression still ignores aliases and goes through `bind_aggregate`.

Duplicate explicit aliases are rejected at bind time, whether or not `ORDER BY` mentions them: `duplicate column alias 'total'`. Two items with the same implicit name remain legal, because a join can already project two columns that share a schema name. Referring to that shared name in `ORDER BY` is the ambiguity error above.

### Where aliases are not visible

`WHERE`, `ON`, and `GROUP BY` resolve source columns only. `WHERE total > 1` and `GROUP BY dept`, when `dept` is only a select alias, fail as an unknown column. Grouping is defined on input columns, and this milestone does not add expression grouping or positional `GROUP BY`.

`HAVING` does not resolve select aliases in this milestone. `HAVING total > 1` is not rewritten to `HAVING COUNT(*) > 1`. It fails with the current `HAVING` binder (unknown column, or a column that is not grouped). That limitation is intentional and should be stated in the README when the milestone is implemented.

Table aliases are unchanged. `FROM employees e` still hides the catalog name `employees`, still qualifies `e.salary`, and is unrelated to a select alias named `e`.

## 5. Aggregate accumulator changes

### Slot shape

`AccValue` is `Copy` and 16 bytes because its variants are `Count(i64)` and `Sum(Option<i64>)`. `update_acc` stages eight of them on the stack and copies them back only after `apply_acc` returns.

`MIN`/`MAX` of `TEXT` own a `String`, so the slot type can no longer be `Copy`. Putting a `String` in the enum also raises every slot, including `COUNT`, to the size of the largest variant. The implementation measured `size_of::<AccValue>()` at 48 bytes and `size_of::<Value>()` at 32 bytes.

Proposed variants:

```rust
enum AccValue {
    Count(i64),
    Sum(Option<i64>),
    Avg { sum: i128, count: i64 },
    Extreme(Option<Value>),
}
```

`AggFn` gains `Avg(usize)`, `Min(usize)`, and `Max(usize)`. `Extreme` serves both `MIN` and `MAX`; the function on `AggFn` decides the direction. A text extreme stores a `Value::Text`. An integer extreme stores `Value::Integer`. Unused variants are not allocated per row beyond the enum itself.

`fresh_acc` sets `Avg { sum: 0, count: 0 }` and `Extreme(None)`.

### Commit protocol

The stack array of `Copy` slots goes away. Replace it with a pending list so a failed aggregate cannot publish an earlier update from the same row, and so an unchanged text extreme is not cloned:

1. For each aggregate in the row, compute the replacement value without writing the live slot.
2. A `NULL` `AVG`/`MIN`/`MAX` input, a `COUNT(column)` of `NULL`, and a `MIN`/`MAX` that does not beat the current extreme produce no pending write.
3. A non-null input that changes the slot appends `(index, new AccValue)` to a pending list. The new text is owned by that pending value.
4. If any aggregate returns an error, drop the pending list and leave the live slots untouched. Do not push a new group when its first row fails. This matches today's `absorb_row` behavior.
5. If every aggregate succeeds, write the pending slots into the live vector.

`COUNT` and `SUM` keep their current overflow checks inside that computation. `AVG` uses the `i128` and `i64` checks from section 2. `MIN` and `MAX` do not overflow.

The statement still returns `Err` and no rows. Tables, the database file, and the WAL are unchanged, because a `SELECT` is `DurableChange::None`. `LIMIT` and `HAVING` still do not stop accumulation, so a later overflow is not hidden by a passing `HAVING` predicate or by `LIMIT 0`.

Eight pending slots fit on the stack as an array of `Option<(usize, AccValue)>` only if `AccValue` is small enough to want that. Prefer a `Vec` with capacity reserved to the aggregate count. The aggregate lists this SQL surface builds are short. The clone cost that matters is a text extreme that actually changes, not the pointer-sized pending list.

### Streaming and joins

`InputRow`, `GroupSink`, `visit_join_pairs`, the hash position buffer, and the choice of nested, hash, or index join stay as they are. The new functions are additional arms in `apply` / materialize. A join aggregate still sees each kept match once, does not store the joined row, and still commits each match's accumulators atomically.

Scalar `AVG`/`MIN`/`MAX` with no `GROUP BY` is one group even when the input is empty, and that group materializes `NULL`. `GROUP BY` over zero rows is zero groups, same as `SUM`.

Materialize writes the ratio or the extreme into the group row at `group_width + slot`. `finish_aggregate` then applies `HAVING`, sorts, applies `LIMIT`, and projects. Those steps already operate on `Value` cells. They need to understand `Value::Ratio` in `compare_values` and in equality for a grouped key only if a ratio could be a group key. It cannot: group keys are input columns. Only aggregate outputs are ratios.

### Shared slots and labels

`agg_same` extends to `Avg`, `Min`, and `Max` by ordinal. `remember_aggregate` still de-duplicates `EXPLAIN` labels by rendered text. `SELECT AVG(salary), AVG(salary)` is one slot and one label. `SELECT AVG(salary), AVG(employees.salary)` is one slot and two labels when the render strings differ. Output names of those two items can still be aliased separately (`a` and `b`) and both `ORDER BY` names point at the same slot's output index.

## 6. Persistence compatibility

No format change.

- `FORMAT_VERSION` stays 2 for a new file without indexes, and 3 when indexes have been migrated. Milestone 12 does not write a version 4.
- Tags stay `NULL = 0`, `INTEGER = 1`, `TEXT = 2`, `BOOLEAN = 3`. There is no ratio tag.
- `encode_value` gains an arm for `Value::Ratio` that returns an internal error (`ratio values are not stored`) and writes nothing. `decode_value` still rejects any other tag as a corrupt file.
- Catalog column codes stay integer, text, and boolean.
- Index keys stay the current scalar encoding. Query results are not indexed.
- `INSERT` and `UPDATE` assign through `DataType` only. A ratio cannot be parsed as a literal and cannot be the type of a column expression, so it never reaches `encode_record` on those statements. The codec arm is the backstop if a later bug tries.

Restarting the process and opening the same file must see the same version, the same rows, and the same query answers. `AVG` does not change a stored salary. A test should run `AVG` against a file, reopen it, and compare bytes of the database file (and an empty WAL after a successful `SELECT`) with the pre-query file.

`Value` is public, and adding a variant is a public match break for external callers. That is acceptable because result rows are `Vec<Value>`. The variant must stay out of the codec, which is the separation between query results and persisted cells. Splitting a second `QueryValue` enum would force every result path, `HAVING`, and `ORDER BY` onto a parallel type while `INSERT` keeps `Value`. One enum with a codec refusal is the smaller fence and keeps `compare_values` in one place.

## 7. SQL execution changes

### Parser (`src/parser.rs`)

`aggregate_call` recognizes `avg`, `min`, and `max` next to `count` and `sum`. Stars are legal only for `COUNT`. `AVG`, `MIN`, `MAX`, and `SUM` of `*` fail. `DISTINCT`, `FILTER`, windows, and nested calls keep their current errors.

`convert_projection` accepts one alias on a column or aggregate item. `convert_having` and `convert_order_by` learn the three functions so `HAVING AVG(salary) > 1` and `ORDER BY MAX(age)` parse. `convert_expr_in` for `WHERE` and `ON` still rejects every aggregate, including the new names, with `aggregates are not allowed in WHERE` or `in ON`.

`GROUP BY` stays a list of columns. An alias is not a group key.

### Binder (`src/expr.rs`, `prepare_groups`)

`AggregateFunction` gains `Avg`, `Min`, and `Max`.

The `HAVING` type scratchpad is a `Vec<DataType>` that fills gaps with `INTEGER`. A ratio does not fit in `DataType`. Replace that scratchpad with a scalar type that is not the schema enum:

```rust
enum ScalarType {
    Integer,
    Text,
    Boolean,
    Ratio,
}
```

Schema columns, records, and indexes keep `DataType`. `note_type` stores `ScalarType`. `COUNT` and `SUM` are `Integer`. `AVG` is `Ratio`. `MIN` and `MAX` copy the argument column's scalar type. Gap fill stays `Integer` only for slots that a column visit has not typed yet, then the aggregate visit overwrites the aggregate's own slot. A ratio slot must not be left as `Integer`.

`check_binary` accepts `Ratio` with `Ratio`, and `Ratio` with `Integer` by promoting the integer. It rejects `Ratio` mixed with `Text` or `Boolean`. Boolean ordering operators stay rejected, including when the boolean came from `MIN` or `MAX`.

`bind_having` otherwise keeps Milestone 11: the root must be boolean or an untyped null, aggregates are `Expr::Aggregate`, and a grouped column is legal.

`prepare_groups` records the output name from the alias or the implicit name, and records the output ordinal from the expression. `group_width` is still captured before `HAVING` and `ORDER BY` append shared aggregates, so those later slots do not shift select indexes.

Non-aggregate `SELECT` uses the alias in `column_names` and teaches `ORDER BY` the same three-step lookup. Sort keys remain ordinals into the output row.

### Executor

`materialize` maps `Avg { count: 0, .. }` and `Extreme(None)` to `Value::Null`. A positive `AVG` count becomes `Value::Ratio`. An extreme is moved out as its `Value`.

`compare_values` gains the ratio comparison from section 2. Two ratios compare exactly. A ratio and an integer compare by promoting the integer. A ratio and any other non-null value is a tie only if a bug reaches the sorter; the binder should have rejected that query. Null ordering is unchanged: default `ASC` is nulls last, `DESC` is nulls first.

`row_matches` is unchanged: a group is kept only when `HAVING` is boolean `TRUE`. `UNKNOWN` from a null average compared with an integer still drops the group.

### EXPLAIN

Single-table and join aggregate `EXPLAIN` keep their current columns. The `aggregates` cell lists rendered calls. `AVG`, `MIN`, and `MAX` appear there, once per distinct render string, whether the call is in the select list, only in `HAVING`, or only in `ORDER BY`. `having` stays `NULL` when there is no `HAVING` clause. The reported join algorithm stays the algorithm execution uses. `EXPLAIN` of a query that would overflow `AVG` succeeds, and the `SELECT` returns the overflow error. `EXPLAIN` does not dirty pages and does not grow the WAL.

Aliases do not appear in `EXPLAIN`. The header of the `SELECT` result is the place the alias is visible.

### Errors that stay

`SELECT *` with grouping or aggregates, nested aggregates, aggregates in `WHERE` or `ON`, unknown columns, ambiguous columns, and `SUM` overflow stay as they are. `SUM` remains an `i64` accumulator. `AVG` does not reuse `SUM`'s slot or its overflow message.

## 8. Testing strategy

Tests live in `tests/milestone12.rs` for SQL, and in `src/executor.rs` or `src/value.rs` where a `cfg(test)` hook or a pure helper is required. Force-join flags stay inside the library tests, as they did for Milestone 11.

### AVG

- `AVG` of `1` and `2` is the ratio `1/2`, displayed `0.5`. `AVG` of `2` and `2` is `2/1`, displayed `2.0`. `AVG` of `-3` and `-2` is `-5/2`.
- One row of `i64::MAX`, one row of `i64::MIN`, and one row of `0` are exact (`n/1`).
- `NULL` inputs are ignored. A mix of `NULL` and `10` averages `10`.
- Empty scalar input and an all-null group return `NULL`, not `0` and not an error.
- Empty `GROUP BY` returns no rows. A scalar empty table returns one `NULL`.
- `AVG(text)` and `AVG(boolean)` fail at bind on an empty table and with `LIMIT 0`.
- Many copies of `i64::MAX` either produce an exact ratio or return `AVG overflows` without a row. A following `SELECT COUNT(*)` still sees every stored row.
- `SUM` and `AVG` of the same column in one select list use different slots. `SUM` can overflow while the mathematical average would have fit in `i64`; the statement still fails if the `SUM` overflows, and a query that is only `AVG` succeeds when the `i128` sum fits.
- Repeated `AVG(salary)` and `AVG(employees.salary)` share a slot. `EXPLAIN` lists each distinct render.

### MIN and MAX

- Integer extremes include duplicates, negatives, `i64::MIN`, and `i64::MAX`. Ties keep a single result.
- Text extremes follow `ORDER BY` on the same column, including the empty string, different cases, and shared prefixes.
- Boolean `MIN` is `false` when any `false` is present, `true` when every non-null value is `true`, and `NULL` when every value is `NULL`. `MAX` is the opposite extreme.
- `WHERE flag > FALSE` still fails. `HAVING MIN(flag) = FALSE` succeeds. `HAVING MIN(flag) > FALSE` fails with the boolean ordering error.
- `MIN` and `MAX` of the same column are two slots. Two identical `MIN` calls are one slot.

### HAVING, joins, limits

- `HAVING AVG(salary) > 1`, `=`, and `IS NULL` use three-valued logic. A null average compared with an integer drops the group. `HAVING TRUE` keeps it.
- The same query over an `INNER JOIN`, with `GROUP BY` a column from each side, matches an independent reference that also ignores nulls and uses integer `i128` division as a ratio.
- Run that reference against nested loop, hash join, hash join with the forced collision, the materialized-pair test hook, and an index join on a migrated file. Both build sides. Self-join. Residual `AND` in `ON`.
- `LIMIT` does not hide a later `AVG` or `COUNT` overflow. `EXPLAIN` of that statement succeeds and does not change the file length or the WAL.

### Aliases

- `ORDER BY total` where `total` aliases `COUNT(*)` matches `ORDER BY COUNT(*)` and does not add a slot.
- `ORDER BY` an alias of a grouped column matches ordering by that column.
- Implicit form `SELECT department dept` has the same header and the same `ORDER BY dept` binding as `AS dept`.
- Unqualified alias beats a source column of the same name. The qualified source name still sees the table column.
- Duplicate explicit alias fails at bind. Two implicit names that collide fail only when `ORDER BY` references them.
- `WHERE`, `ON`, `GROUP BY`, and `HAVING` do not accept the alias. Table alias scoping tests from Milestone 11 still pass unchanged.
- A non-aggregate `SELECT name AS n FROM users ORDER BY n` works.

### Persistence and EXPLAIN

- Encode and decode of `NULL`, integers (including `i64::MIN`), text, and booleans still round-trip. Encoding a ratio returns an error and does not bump a version constant.
- Open a file, run `AVG` / `MIN` / `MAX`, reopen, and read the same rows. File version is unchanged.
- `EXPLAIN` reports `AVG`, `MIN`, and `MAX` once for a repeated render, reports a `HAVING`-only aggregate, and names the same join algorithm the executor uses. Invalid SQL returns the same binding error from `EXPLAIN` and `SELECT`.

### Randomized reference

Extend the Milestone 11 generator. For each recorded seed (`0x0a99`, `0x51c0`, `0xc0ff`, `0x5eed`, `0xa11c`, `0x11ab`) and a handful of fresh seeds, compare the engine with a reference that:

- drops non-matching join keys, treating `NULL` as not equal
- groups with `NULL` equal to `NULL`
- averages with a checked `i128` sum and a reduced ratio
- takes min and max with `Ord` on `i64`, `String`, and `bool`
- applies `HAVING` only when the predicate is true
- sorts with the engine's nulls-last / nulls-first rule

Checksum the group count, the ratio numerator and denominator, and the min/max text so a display-only change cannot hide a wrong quotient.

## 9. Benchmark plan

The release run is recorded in [benchmarks.md](benchmarks.md). The harness below is what that run executed. Cold is the first sample. Warm is the median of the next five. `bytes` uses `size_of` and is not process RSS.

Add an ignored library profile, run with:

```text
cargo test --release --offline --lib profile_milestone12 -- --ignored --nocapture
```

Sizes: 1,000, 10,000, and 100,000 rows.

Shapes, each on the same integer column so the scan cost is shared:

- `SUM` versus `AVG`, scalar. The difference is the `i128` add and the final reduction, not a different scan.
- `GROUP BY` a 10-value key, and `GROUP BY` the primary key (one group per row). Report groups times `size_of::<AccValue>()` times the number of slots. The Milestone 11 estimate of 16 bytes per `COUNT`/`SUM` slot is the pre-change size; recompute it after `AccValue` grows.
- `MIN` and `MAX` on short text and on a long text column, grouped at high cardinality. This is the cost of cloning a `String` when the extreme changes. An unchanged extreme should not allocate.
- The Milestone 11 join shape (users and orders, hash and nested, low and high cardinality) with `AVG` instead of `SUM`, plus one `HAVING AVG(...) > 0` run. Streaming stays on. Record the position-buffer size separately from accumulator bytes, as Milestone 11 did.
- One materialized-pair run at 10,000 rows, behind the existing test hook, so the write-up can separate join-algorithm time from pair-materialization time. Do not add a spill path.

Report checksums that include the ratio numerator and denominator. A faster wrong average is not a result.

Memory notes to verify, not to invent:

- One `AVG` slot is larger than one `SUM` slot because of the `i128`.
- A text `MIN` slot holds one `String` per group, not one per input row.
- High-cardinality grouping is one group per distinct key, same as Milestone 11. `AVG` does not add a per-input-row allocation on the hash map.
- The outer-build hash join still buffers one pair of positions per kept match. This milestone does not change that buffer.

## 10. Implementation phases

1. **Ratio value.** Add `Value::Ratio`, reduction, decimal-or-fraction `Display`, and the exact comparison helper. Point `encode_value` at an error arm. Unit-test reduction, `i64::MIN`, terminating and non-terminating display, and codec refusal. No SQL yet.
2. **Parser and binder.** Recognize `AVG`, `MIN`, and `MAX`. Introduce `ScalarType`. Type `AVG` as `Ratio` and `MIN`/`MAX` as the argument type. Reject bad argument types, stars, and aliases-not-yet-accepted still until phase 4. `EXPLAIN` labels render the new calls.
3. **Accumulators.** Add the `AggFn` and `AccValue` variants. Replace the `Copy` stack stage with the pending-slot commit. Materialize nulls and ratios. Wire scalar scan, `GROUP BY`, `HAVING`, `ORDER BY`, `LIMIT`, and the existing join visitors. Keep alias rejection in this phase so accumulator tests are not mixed with name resolution.
4. **Aliases.** Accept one alias in the projection, use it as the header, and resolve unqualified `ORDER BY` names with the precedence in section 4. Leave `WHERE`, `ON`, `GROUP BY`, and `HAVING` on source columns.
5. **Tests.** Add the SQL suite and the randomized reference, including the file restart and `EXPLAIN` cases.
6. **Measurement.** Run the profile, then update `docs/benchmarks.md` and the README limitations with the behavior that shipped. Do not put estimated timings in those files.

Verification for the implementation, when it happens: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test`. This design document does not run them, because it does not change Rust sources.

## 11. Risks and unresolved decisions

### Decisions to review

These are recommendations, not settled product rules.

1. **Result type.** Query-only reduced ratio (`i128` / `i64`), built from an `i128` sum. Not `f64`, and not a new persisted numeric. Display is the shortest exact decimal when the fraction terminates (`4/1` as `4.0`), and the reduced `numerator/denominator` text when it does not (`1/3`). The alternative is a rounded fixed scale for every average, which is easier to read and is not exact.
2. **Alias precedence.** An unqualified `ORDER BY` name that matches one output alias uses the alias even when a source column has the same name. Qualified names ignore aliases. Duplicate explicit aliases fail at bind. The alternative is "source column wins" or "ambiguous whenever both exist", which makes `ORDER BY total` fail on common names.
3. **Boolean `MIN`/`MAX`.** Allowed, using `false < true`. Boolean ordering operators in `WHERE` and `HAVING` stay rejected. The alternative is to reject `MIN(boolean)` so aggregates and predicates share one restriction.
4. **Accumulator representation.** One non-`Copy` enum and a pending-slot commit, accepting a larger slot for `COUNT` and `SUM`. The alternative is two accumulator vectors (numeric `Copy` slots, and owned extremes beside them) to keep `COUNT` at 16 bytes. That splits the atomic commit path.
5. **`HAVING` aliases.** Not in this milestone. `HAVING total > 1` will not see `COUNT(*) AS total`.

### Risks

- **Cross-product overflow in comparison.** The 256-bit product in section 2 is checked against an independent schoolbook integer, including `i64::MIN`, negative denominators, and numerators near `i128::MAX`. A comparison that casts to `f64` would undo the representation decision.
- **`data_type()` returns `None` for both `NULL` and a ratio.** Call sites that treat `None` as null must not be reused for ratios. The binder uses `ScalarType` instead. A test should assign nothing and insert nothing from an `AVG` result.
- **Public `Value` variant.** External matches gain a new arm. The codec must remain exhaustive so a ratio cannot fall through to a tag.
- **Text `MIN` clones.** A group whose extreme changes every row copies a `String` per change. The pending-slot rule avoids copying it when it does not change. High-cardinality long text is the benchmark that shows the cost.
- **`EXPLAIN` label versus slot.** Different renders of one shared call still produce two labels. Tests should assert the slot count separately from the label string, or a shared `AVG(salary)` / `AVG(employees.salary)` will look like two averages in `EXPLAIN` while computing one.
- **Implicit duplicate headers.** Joins that project the same column name twice stay legal and become ambiguous only in `ORDER BY`. That is a behavior change for `ORDER BY name` if both outputs are implicitly named `name`: the error is the alias ambiguity, not the source-column ambiguity. The message should stay explicit.
- **No disk spill.** `AVG` does not make high-cardinality grouping bounded. The position buffer for an outer-build hash join is still proportional to the match count.
