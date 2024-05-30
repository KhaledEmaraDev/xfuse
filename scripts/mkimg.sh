#! /bin/sh -e

# Recreate the golden image used for the integration tests

mkfiles() {
	DIR=$1
	COUNT=$2

	mkdir $DIR
	seq -f "%06g" 0 $(( COUNT - 1 )) | xargs -I % touch "$DIR/frame%"
}

# Make a directory filled with files that have very long file names
mkfiles2() {
	DIR=$1
	COUNT=$2

	mkdir $DIR
	seq -f "%08.0f" 0 $(( COUNT - 1 )) | xargs -I % touch "$DIR/frame__________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________%"
}

# Make a directory filled with 40 files whose names include hash collisions
mk_colliding_files() {
	DIR=$1

	mkdir $DIR
	for f in 210001 2a0004 310009 81000a \
		210004 2a0001 3a0009 81000d \
		210005 2a0000 3a0008 81000e\
		210011 2a0014 310019 81001a\
		210014 2a0011 3a0019 81001d\
		210015 2a0010 3a0018 81001e\
		210021 2a0024 310029 81002a\
		210024 2a0021 3a0029 81002d\
		210025 2a0020 3a0028 81002e\
		210031 2a0034 310039 81003a; do
		touch $DIR/$f
	done
}

mkattrs() {
	FILE=$1
	COUNT=$2
	REMOTE=$3

	touch $FILE
	seq -f "%06g" 0 $(( COUNT - 1 )) | xargs -I % \
		setfattr -n user.attr.% -v value.% $FILE
	seq -f "%06g" 0 $(( REMOTE - 1 )) | xargs -I % \
		setfattr -n user.remote_attr.$i -v _______________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________.% $FILE
}

# Create a file with many xattrs.  Use remote xattrs only, and try to fragment
# the attribute fork as much as possible.
mkattrs2() {
	FILE=$1
	COUNT=$2

	touch $FILE
	for i in `seq -f "%06g" 0 $(( COUNT - 1 ))`; do
		setfattr -n user.remote_attr.$i -v _______________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________.$i $FILE
		setfattr -n user.remote_attr.$i.X -v _______________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________.XXXXXX $FILE
	done
	for i in `seq -f "%06g" 0 $(( COUNT - 1 ))`; do
		setfattr -x user.remote_attr.$i.X $FILE
	done
}

# Write a file that has as many fragments as possible.  Each 16-byte line of
# the file will contain the byte offset in ASCII.
write_fragmented_file() {
	FILE=$1
	BSIZE=$2
	EXTENTS=$3

	for i in `seq 0 $(( $EXTENTS - 1 ))`; do
		jot -n -w %016x -s "" $(( $BSIZE / 16 )) $(( i * $BSIZE )) $(( ( $i + 1 ) * $BSIZE )) 16 >> $FILE
		if [ "$i" -lt $(( $EXTENTS - 1 )) ]; then
			jot -n -b X -s "" $BSIZE 0 >> $FILE
		fi
	done
	if [ "$EXTENTS" -gt 1 ]; then
		# Use fallocate's collapse function to force the file to be
		# allocated in multiple small extents, rather than one big one.
		for i in `seq 0 $(( $EXTENTS - 2 ))`; do
			fallocate -c -o $(( ( $i + 1 ) * $BSIZE )) -l $BSIZE $FILE
		done
	fi
}

# Write a file that has as few fragments as possible.  Each 16-byte line of the
# file will contain the byte offset in ASCII.
write_sequential_file() {
	FILE=$1
	FSIZE=$2

	jot -n -w %016x -s "" $(( $FSIZE / 16 )) 0 $FSIZE 16 >> $FILE
}

mkfs_4096() {
	truncate -s 96m resources/xfs4096.img
	# Create a disk image with block size 4096 (the default) and dir size 8192.
	# Accessing certain code paths requires the directory size to be larger than
	# the block size.
	mkfs.xfs --unsupported -n size=8192 -f resources/xfs4096.img
	MNTDIR=`mktemp -d`
	mount -t xfs resources/xfs4096.img $MNTDIR

	# Create directories with various internal storage formats
	#
	# With 4k blocks and 8k directories
	# nfiles	namelen	format	bmbt.level	keys_in_inode
	# 2		11	sf
	# 32		11	block
	# 384		11	leaf
	# 1024		11	node
	# 8192		11	btree	1		1
	# 2000000	11	btree	1		16
	# 8192		255	btree	1		1
	# 16384		255	btree	1		1
	# 32768		255	btree	1		2
	# 65536		255	btree	1		4
	# 131072	255	btree	1		7
	# 262144	255	btree	1		12
	# 524288	255	btree	2		1
	# 1048576	255	btree	2		1
	# 2097152	255	btree	2		1
	mkfiles ${MNTDIR}/sf 2
	mkfiles ${MNTDIR}/block 32
	mkfiles ${MNTDIR}/leaf 384

	# Make a block directory with hash collisions.
	# shortform directories cannot have hash collisions (because they don't use hashes).
	# the other directory types are large enough that hash collisions
	# happen organically in "mkfiles"
	mk_colliding_files ${MNTDIR}/block-with-hash-collisions

	mkdir ${MNTDIR}/xattrs
	mkattrs ${MNTDIR}/xattrs/local 4 0
	mkattrs ${MNTDIR}/xattrs/extents 64 0

	mkdir ${MNTDIR}/links
	ln -s dest ${MNTDIR}/links/sf
	ln -s 0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDE ${MNTDIR}/links/max

	mkdir ${MNTDIR}/files
	echo "Hello, World!" > ${MNTDIR}/files/hello.txt
	touch -t  198209220102.03 ${MNTDIR}/files/hello.txt # Set mtime to my birthday
	touch -at 201203230405.06 ${MNTDIR}/files/hello.txt # Set atime to my kid's birthday
	ln ${MNTDIR}/files/hello.txt ${MNTDIR}/files/hello2.txt
	chown 1234:5678 ${MNTDIR}/files/hello.txt
	chmod 01234 ${MNTDIR}/files/hello.txt

	touch ${MNTDIR}/files/executable
	chmod 755 ${MNTDIR}/files/executable

	touch -t 191811111111.11 ${MNTDIR}/files/old.txt    # Armistice day
	mkfifo ${MNTDIR}/files/fifo
	python3 -c "import socket as s; sock = s.socket(s.AF_UNIX); sock.bind('${MNTDIR}/files/sock')"
	mknod ${MNTDIR}/files/blockdev b 1 2
	mknod ${MNTDIR}/files/chardev c 1 2

	# Now create some files that contain data.  Fill each file with an array of
	# 16-byte ASCII strings.  Each string contains the address, in ASCII, of its
	# starting position.  Use ASCII because it's easy to create from a shell
	# script.
	# With 4k blocks and 8k directories
	# extents	format	bmbt.level	keys_in_inode
	# 1		extents			1
	# 4		extents			4
	# 16		btree	1		1
	# 2048		btree	1		9
	# 4096		btree	2		1
	write_sequential_file ${MNTDIR}/files/large_extent.txt 1048576
	write_fragmented_file ${MNTDIR}/files/partial_extent.txt 8448 1
	write_fragmented_file ${MNTDIR}/files/single_extent.txt 4096 1
	write_fragmented_file ${MNTDIR}/files/four_extents.txt 4096 4
	write_fragmented_file ${MNTDIR}/files/btree2.txt 4096 16
	write_fragmented_file ${MNTDIR}/files/btree2.4.txt 4096 2048
	write_fragmented_file ${MNTDIR}/files/btree3.txt 4096 4096

	# Now create some sparse files
	truncate -s 1T ${MNTDIR}/files/sparse.fully.txt
	write_fragmented_file ${MNTDIR}/files/sparse.extents.txt 4096 4
	fallocate -p -o 0 -l 4096 ${MNTDIR}/files/sparse.extents.txt
	fallocate -p -o 8192 -l 4096 ${MNTDIR}/files/sparse.extents.txt
	write_fragmented_file ${MNTDIR}/files/sparse.btree.txt 4096 16
	fallocate -p -o 0 -l 4096 ${MNTDIR}/files/sparse.btree.txt
	fallocate -p -o 8192 -l 4096 ${MNTDIR}/files/sparse.btree.txt
	write_fragmented_file ${MNTDIR}/files/hole_at_end.txt 4096 4
	truncate -s 20480 ${MNTDIR}/files/hole_at_end.txt 

	# Create a pair of reflinked files
	write_sequential_file ${MNTDIR}/files/reflink_a.txt 16384
	cp --reflink=always ${MNTDIR}/files/reflink_a.txt ${MNTDIR}/files/reflink_b.txt
	cp --reflink=always ${MNTDIR}/files/reflink_a.txt ${MNTDIR}/files/reflink_partial.txt
	dd if=${MNTDIR}/files/four_extents.txt bs=4096 count=1 conv=notrunc of=${MNTDIR}/files/reflink_partial.txt
	dd if=${MNTDIR}/files/four_extents.txt bs=4096 count=2 iseek=2 oseek=2 conv=notrunc of=${MNTDIR}/files/reflink_partial.txt
	
	# Create a directory containing files of every possible name length
	mkdir ${MNTDIR}/all_name_lengths
	for i in `seq 1 255`; do
		touch ${MNTDIR}/all_name_lengths/`printf "%0${i}d" ${i}`
	done

	umount ${MNTDIR}
	rmdir $MNTDIR
	zstd -f resources/xfs4096.img
}

mkfs_512() {
	# Create a 2nd image with smaller block size and directory size, to
	# test larger files and directories without using so much disk space.
	truncate -s 600m resources/xfs1024.img
	# 1024 is the smallest allowed blocksize for a V5 file system, and 4k
	# is the smallest allowed directory size.
	mkfs.xfs --unsupported -b size=1024 -n size=4096 -f resources/xfs1024.img
	MNTDIR=`mktemp -d`
	mount -t xfs resources/xfs1024.img $MNTDIR

	# Create directories with various internal storage formats
	#
	# With 512B blocks and 4k directories
	# nfiles	namelen	format	bmbt.level	keys_in_inode
	# 512		255	btree	1		1
	# 1024		255	btree	1		1
	# 2048		255	btree	1		2
	# 4096		255	btree	1		4
	# 8192		255	btree	1		6
	# 16384		255	btree	2		1
	##
	# With 1k blocks and 4k directories
	# nfiles
	# 64		255	leaf	N/A		5 data 1 leaf
	# 256		255	leaf	N/A		6 data 1 leaf
	# 384		255	leaf	N/A		7 data 1 leaf
	# 448		255	leaf	N/A		8 data 1 leaf
	# 480		255	leaf	N/A		8 data l leaf
	# 496		255	node	N/A		8 data 2 leaf
	# 512		255	node	N/A		9 data 3 leaf
	# 4096		255	btree	1		2
	# 8192		255	btree	1		3
	# 16384		255	btree	1		5
	# 32768		255	btree	1		9
	# 65536		255	btree	1		18
	# 131072	255	btree	2		1
	# 262144	255	btree	2		2
	mkfiles2 ${MNTDIR}/leaf 256
	mkfiles2 ${MNTDIR}/node1 496
	mkfiles2 ${MNTDIR}/node3 512
	mkfiles2 ${MNTDIR}/btree2.3 8192
	mkfiles2 ${MNTDIR}/btree3 131072

	# Create files with various sizes of btree extent lists.
	# With 512B blocks and 4k directories
	# extents	format	bmbt.level	keys_in_inode
	# 16		btree	1		1
	# 64		btree	1		3
	# 256		btree	1		9
	# 1024		btree	2		2
	# 2048		btree	2		3
	# 4096		btree	2		5
	#
	# With 1k blocks and 4k directories
	# extents	format	bmbt.level	keys_in_inode
	# 64		btree	1		2
	# 1024		btree	1		18
	# 2048		btree	2		1
	# 4096		btree	2		2
	# 8192		btree	2		3
	# 16384		btree	2		5
	# 32768		btree	2		10
	# 65536		btree	2		19
	mkdir ${MNTDIR}/files
	write_sequential_file ${MNTDIR}/files/large_extent.txt 1048576
	write_fragmented_file ${MNTDIR}/files/btree2.2.txt 1024 64
	write_fragmented_file ${MNTDIR}/files/btree3.txt 1024 2048
	write_fragmented_file ${MNTDIR}/files/btree3.3.txt 1024 8192

	# Create a regular file that also has an xattr
	write_fragmented_file ${MNTDIR}/files/btree2_with_xattrs.txt 1024 64
	setfattr -n user.foo -v bar ${MNTDIR}/files/btree2_with_xattrs.txt

	# Allocate files with BTree extent lists for its xattrs.  Use remote
	# attributes only so we can force the address fork to be highly
	# fragmented.
	#
	# attrs		di_aformat	btree.level	keys_in_root
	# 4		extents				5
	# 8		extents				9
	# 16		btree		1		1
	# 32		btree		1		1
	# 64		btree		1		2
	# 128		btree		1		3
	# 256		btree		1		5
	# 512		btree		2		1
	# 1024		btree		2		1
	# 2048		btree		2		1
	# 4096		btree		2		2
	# 8192		btree		2		3
	# 16384		btree		2		9
	mkdir ${MNTDIR}/xattrs
	mkattrs2 ${MNTDIR}/xattrs/btree2 16
	mkattrs2 ${MNTDIR}/xattrs/btree2.5 256
	mkattrs2 ${MNTDIR}/xattrs/btree3 512

	# Create a BTree that also has xattrs
	mkfiles2 ${MNTDIR}/btree2.with-xattrs 1024
	mkattrs ${MNTDIR}/btree2.with-xattrs 1 0

	umount ${MNTDIR}
	rmdir $MNTDIR
	zstd -f resources/xfs1024.img
}

mkfs_preallocated() {
	# Create a small image to test preallocated files.  It needs a
	# dedicated image because we must fill the file with garbage data in
	# order to verify that we don't read garbage back.
	jot -b X -s "" -n 16777216 > resources/xfs_preallocated.img
	mkfs.xfs --unsupported -f resources/xfs_preallocated.img
	MNTDIR=`mktemp -d`
	mount -t xfs resources/xfs_preallocated.img $MNTDIR

	mkdir ${MNTDIR}/files
	touch ${MNTDIR}/files/preallocated
	fallocate -l 8m ${MNTDIR}/files/preallocated

	umount ${MNTDIR}
	rmdir $MNTDIR
	zstd -f resources/xfs_preallocated.img
}

mkfs_4096
mkfs_512
mkfs_preallocated
