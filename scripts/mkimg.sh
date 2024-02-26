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

mkattrs() {
	FILE=$1
	COUNT=$2
	REMOTE=$3

	touch $FILE
	seq -f "%06g" 0 $(( COUNT - 1 )) | xargs -I % \
		setfattr -n user.attr.% -v value.% $FILE
	seq -f "%06g" 0 $(( REMOTE - 1 )) | xargs -I % \
		setfattr -n user.remote_attr.% -v ________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________________.% $FILE
}

fill_file() {
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
	mkfiles ${MNTDIR}/node 1024

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
	fill_file ${MNTDIR}/files/single_extent.txt 4096 1
	fill_file ${MNTDIR}/files/four_extents.txt 4096 4
	fill_file ${MNTDIR}/files/btree2.txt 4096 16
	fill_file ${MNTDIR}/files/btree2.4.txt 4096 2048
	fill_file ${MNTDIR}/files/btree3.txt 4096 4096

	# Now create some sparse files
	truncate -s 1T ${MNTDIR}/files/sparse.fully.txt
	fill_file ${MNTDIR}/files/sparse.extents.txt 4096 4
	fallocate -p -o 0 -l 4096 ${MNTDIR}/files/sparse.extents.txt
	fill_file ${MNTDIR}/files/sparse.btree.txt 4096 16
	fallocate -p -o 0 -l 4096 ${MNTDIR}/files/sparse.btree.txt
	fill_file ${MNTDIR}/files/hole_at_end.txt 4096 4
	truncate -s 20480 ${MNTDIR}/files/hole_at_end.txt 

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
	# 4096		255	btree	1		2
	# 8192		255	btree	1		3
	# 16384		255	btree	1		5
	# 32768		255	btree	1		9
	# 65536		255	btree	1		18
	# 131072	255	btree	2		1
	# 262144	255	btree	2		2
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
	fill_file ${MNTDIR}/files/btree2.2.txt 1024 64
	fill_file ${MNTDIR}/files/btree3.txt 1024 2048
	fill_file ${MNTDIR}/files/btree3.3.txt 1024 8192

	# Allocate a file with a BTree extent list for its xattrs.  This is
	# unreliable; more xattrs don't necessarily result in a BTree extent
	# list.  After changing this script, double check that it's still a
	# btree using xfs_db.
	# attrs		format	bmbt.level	keys_in_inode
	# 64		extents			5
	# 128		extents			9
	# 256		btree	1		1
	# 512		btree	1		1
	# 1024		btree	1		2
	# 2048		btree	1		3
	# 4096		btree	1		5
	# 8192		btree	2		1
	mkdir ${MNTDIR}/xattrs
	mkattrs ${MNTDIR}/xattrs/btree2 256 1
	mkattrs ${MNTDIR}/xattrs/btree2.3 2048 1
	mkattrs ${MNTDIR}/xattrs/btree3 8192 1

	umount ${MNTDIR}
	rmdir $MNTDIR
	zstd -f resources/xfs1024.img
}

mkfs_4096
mkfs_512
