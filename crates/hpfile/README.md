# Head-prunable file

Normal files can not be pruned\(truncated\) from the beginning to some middle position. A `HPFile` use a sequence of
small files to simulate one big virtual file. Thus, pruning from the beginning is to delete the first several small
files.

A `HPFile` can only be read and appended. Any byteslice which was written to it is immutable.

To append a new byteslice into a `HPFile`, use the `append` function, which will return the start position of this
byteslice. Later, just pass this start position to `read_at` for reading this byteslice out. The position passed to
`read_at` must be the beginning of a byteslice that was written before, instead of its middle. Do NOT try to read the
later half (from a middle point to the end) of a byteslice.

A `HPFile` can also be truncated: discarding the content from a given position to the end of the file. During trucation,
several small files may be removed and one small file may get truncated.

A `HPFile` can serve many reader threads. If a reader thread just read random positions, plain `read_at` is enough. If a
reader tends to read many adjacent byteslices in sequence, it can take advantage of spatial locality by using
`read_at_with_pre_reader`, which uses a `PreReader` to read large chunks of data from file and cache them. Each reader
thread can have its own `PreReader`. A `PreReader` cannot be shared by different `HPFile`s.

A `HPFile` can serve only one writer thread. The writer thread must own a write buffer that collects small pieces of
written data into one big single write to the underlying OS file, to avoid the cost of many syscalls writing the OS
file. This write buffer must be provided when calling `append` and `flush`. It is owned by the writer thead, instead of
`HPFile`, because we want `HPFile` to be shared between many reader threads.
