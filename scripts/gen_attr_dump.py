#!/usr/bin/env python3

import math
import argparse


parser = argparse.ArgumentParser(
    usage="%(prog)s -fnl pathname...",
    description="Generate setfattr dump to be used with `--restore`.",
)
parser.add_argument("-f", "--file-name", type=str, required=True)
parser.add_argument("-n", "--number-of-attributes", type=int, required=True)
parser.add_argument("-l", "--attribute-value-length", type=int, required=True)
args = vars(parser.parse_args())

number_of_digits = int(math.log10(args["number_of_attributes"])) + 1

with open("attr_dump", "w+") as dump_file:
    dump_file.write(f"# file: {args['file_name']}\n")

    for i in range(args["number_of_attributes"]):
        dump_file.write(
            f"user.attribute_{i:0{number_of_digits}}=0x{'ff' * args['attribute_value_length']}\n"
        )
