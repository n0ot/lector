# Tables

Lector can move through tabular output one cell at a time and tell you the row
and column you are in. Table navigation lives inside [Review](review.md).

## What counts as a table

- **Pipe tables** that separate columns with `|`, with or without a border.
- **Fixed-width tables** whose columns are separated by blank space.
- **Anything else**, if you mark the columns yourself. See
  [Mark columns by hand](#mark-columns-by-hand).

## Navigate a table

1. Press `M-r` to open Review.
2. Move the cursor onto the table.
3. Press `gt`. Lector detects the table around the cursor.
4. Move by cell:

| Key | Action |
| --- | --- |
| `]\|` | Next cell, wrapping to the next row |
| `[\|` | Previous cell, wrapping to the previous row |
| `}\|` | Same column, row below |
| `{\|` | Same column, row above |

Each move speaks the row, the column name, and the cell.

Ordinary Review keys keep working. Lector tells you when a move crosses a row,
a column, or the edge of the table. If you wander out of the table, the next
cell key brings you back in.

The table stays active until you close Review or press `gt` or `gT` again.
Pressing `gt` always starts fresh: if no table is found, none is active.

## Row headers

Often the first column names each row. To hear it as you move between rows:

1. Move to a cell in that column.
2. Press `gH`.

Now a row change speaks the row number, the row header, the column, and the
cell. Press `gH` on the same column to turn row headers off, or on another
column to use that one instead.

## Mark columns by hand

Use this when detection gets the columns wrong.

1. In Review, move to the table's header row, or its first data row, and press
   `gT`.
2. Move to the start of each column and press `Space` to set a tab stop. The
   first tab stop is the table's left edge. The last column runs to the end of
   the line.
3. Press `Enter` to save and start navigating, or `Escape` to cancel.

Optional steps before you press `Enter`:

- **Headers.** Press `H` to switch between "the first row holds the column
  names" and "there is no header row".
- **Column names.** Press `c` in a column to name it. `Enter` saves the name,
  `Escape` discards it, and `C-u` clears it. Unnamed columns are announced as
  "column 1", "column 2", and so on.
- **Bottom edge.** Move to the last data row and press `gB`.
- **Right edge.** Move to the last column that belongs to the table and press
  `gR`.

Pressing `gB` or `gR` again on its marker removes it.

Your column layout, names, and row-header choice last until you close Review
or start another table.
