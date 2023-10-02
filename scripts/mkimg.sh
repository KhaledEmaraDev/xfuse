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
mkfs.xfs -n size=8192 -f resources/xfs.img
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

umount ${MNTDIR}

rmdir $MNTDIR

zstd -f resources/xfs.img
