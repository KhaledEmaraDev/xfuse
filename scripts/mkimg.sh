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

truncate -s 32m resources/xfs.img
mkfs.xfs -n size=8192 -f resources/xfs.img
MNTDIR=`mktemp -d`
mount -t xfs resources/xfs.img $MNTDIR
mkfiles ${MNTDIR}/sf 2
mkfiles ${MNTDIR}/block 32
mkfiles ${MNTDIR}/leaf 256
mkfiles ${MNTDIR}/node 1024
mkfiles ${MNTDIR}/btree 8192
umount ${MNTDIR}

rmdir $MNTDIR

zstd -f resources/xfs.img
