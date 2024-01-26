#! /bin/sh -e

# Recreate the golden image used for the integration tests

mkfiles() {
	DIR=$1
	COUNT=$2

	mkdir $DIR
	for i in $(seq -f "%06g" 0 $(( COUNT - 1 )) ); do
		touch "$DIR/frame${i}"
	done
}

mkattrs() {
	FILE=$1
	COUNT=$2

	touch $FILE
	for i in $(seq -f "%06g" 0 $(( COUNT - 1 )) ); do
		setfattr -n user.attr.${i} -v value.${i} $FILE
	done
}

truncate -s 32m resources/xfs.img
mkfs.xfs --unsupported -n size=8192 -f resources/xfs.img
MNTDIR=`mktemp -d`
mount -t xfs resources/xfs.img $MNTDIR

mkfiles ${MNTDIR}/sf 2
mkfiles ${MNTDIR}/block 32
mkfiles ${MNTDIR}/leaf 384
mkfiles ${MNTDIR}/node 1024
mkfiles ${MNTDIR}/btree 8192

mkdir ${MNTDIR}/xattrs
mkattrs ${MNTDIR}/xattrs/local 4
mkattrs ${MNTDIR}/xattrs/extents 64
# TODO: figure out how to force the xattrs to be allocated as a btree.
# Sequentially allocating as many ask 256k xattrs doesn't do it.

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
jot -n -w %016x -s "" 5 0 80 16 > ${MNTDIR}/files/single_extent.txt

# Use fallocate's collapse function to force the file to be allocated in
# multiple small extents, rather than one big one.
jot -n -w %016x -s "" 256 0 4096 16 > ${MNTDIR}/files/four_extents.txt
jot -n -b X -s "" 4096 0 >> ${MNTDIR}/files/four_extents.txt
jot -n -w %016x -s "" 256 4096 8192 16 >> ${MNTDIR}/files/four_extents.txt
jot -n -b X -s "" 4096 0 >> ${MNTDIR}/files/four_extents.txt
jot -n -w %016x -s "" 256 8192 12288 16 >> ${MNTDIR}/files/four_extents.txt
jot -n -b X -s "" 4096 0 >> ${MNTDIR}/files/four_extents.txt
jot -n -w %016x -s "" 256 12288 16384 16 >> ${MNTDIR}/files/four_extents.txt
fallocate -c -o 4096 -l 4096 ${MNTDIR}/files/four_extents.txt
fallocate -c -o 8192 -l 4096 ${MNTDIR}/files/four_extents.txt
fallocate -c -o 12288 -l 4096 ${MNTDIR}/files/four_extents.txt

for i in `seq 0 16`; do
	jot -n -w %016x -s "" 256 $(( i * 4096 )) $(( ( $i + 1 ) * 4096 )) 16 >> ${MNTDIR}/files/btree.txt
	jot -n -b X -s "" 4096 0 >> ${MNTDIR}/files/btree.txt
done
for i in `seq 0 15`; do
	fallocate -c -o $(( ( $i + 1 ) * 4096 )) -l 4096 ${MNTDIR}/files/btree.txt
done
 

umount ${MNTDIR}

rmdir $MNTDIR

zstd -f resources/xfs.img
