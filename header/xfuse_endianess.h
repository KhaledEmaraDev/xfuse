#ifndef _XFUSE_ENDIANNESS_H
#define _XFUSE_ENDIANNESS_H

#include <byteswap.h>
#include <endian.h>

#if __BYTE_ORDER == __LITTLE_ENDIAN

#define __be16_to_host(x) bswap_16(x)
#define __be32_to_host(x) bswap_32(x)
#define __be64_to_host(x) bswap_64(x)

#else

#define __be16_to_host(x) (x)
#define __be32_to_host(x) (x)
#define __be64_to_host(x) (x)

#endif

#define be16_to_host(x) (uint16_t) __be16_to_host((uint16_t)(x))
#define be32_to_host(x) (uint32_t) __be32_to_host((uint32_t)(x))
#define be64_to_host(x) (uint64_t) __be64_to_host((uint64_t)(x))

#endif /* defined _XFUSE_ENDIANNESS_H */
