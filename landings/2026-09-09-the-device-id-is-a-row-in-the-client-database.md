# The device id is a row in the client's database

`desk-device` is gone. This client's desk device id is a row in
`rho-client.redb`, in the desk replica's own tables
(`gui_desk_device_v1`), minted on the first launch that has a database
and read from it ever after.

The question was the user's: why does `desk-device` exist, can't it be
the client db? It can, and it has to be. The id and the replica are one
thing. A version is a count of *this device's* writes inside one store,
and the daemon counts them too. Delete the database while the id
survives beside it and the fresh replica starts writing stamps the
daemon has already seen: last-writer-wins drops every one of them, in
silence, and the user's verdict simply does not happen. In the file, the
id dies with the replica whose writes it counts, and a database with no
device row is a new device with nothing behind it.

So there is no migration and no fallback read of the old file. A client
whose database is intact keeps the id it already had in the file only if
that file was copied — it was not, so every client mints once more, and
that is correct: it is a new replica.

**Nothing waits for the file.** The database opens on the model thread,
because after an unclean stop redb rebuilds its allocator from every
page, and no frame may wait on that. So the id is read where the replica
is read, from the same file: `DeskCells` holds no device until something
needs one, and the first thing that does is the sync that follows the
replica load. Before that there is no replica to count in either, and
the answer is an id of this process's own — which is what a test wants
anyway, since several GUIs in one process are several devices.

**QA.** The rig allow lists drop `desk-device`; a rig that copies
`rho-client.redb` is the same device as the client it copied, and one
that does not is a new device, which is what a rig wants anyway.
