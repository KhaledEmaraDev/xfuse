#! /bin/sh -e

# Recreate the golden image used for the integration tests

mkfiles() {
	DIR=$1
	COUNT=$2

	mkdir $DIR
	for i in $(seq -f "%06g" 0 $(( COUNT - 1 )) ); do
		mkdir "$DIR/frame${i}"
	done
}

truncate -s 512m resources/xfs.img
mkfs.xfs -n size=16384 -f resources/xfs.img
MNTDIR=`mktemp -d`
mount -t xfs resources/xfs.img $MNTDIR
mkfiles ${MNTDIR}/sf 4
mkfiles ${MNTDIR}/block 8
mkfiles ${MNTDIR}/leaf 256
mkfiles ${MNTDIR}/node 2048
mkfiles ${MNTDIR}/btree 204800
umount ${MNTDIR}

rmdir $MNTDIR

zstd resources/xfs.img
