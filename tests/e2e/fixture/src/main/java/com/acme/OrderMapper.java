package com.acme;

import org.apache.ibatis.annotations.Mapper;
import org.apache.ibatis.annotations.Param;

/** The MyBatis mapper interface. Every method's SQL is in the XML beside it,
 *  which is where the table it reads is written down. */
@Mapper
public interface OrderMapper {
    String findById(@Param("id") String id);

    void insert(@Param("sku") String sku);
}
