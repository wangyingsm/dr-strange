package com.acme;

import org.apache.dubbo.config.annotation.DubboReference;

/** The Dubbo consumer: it holds the interface and never the implementation,
 *  so only a per-call match can say what answers it. */
public class Checkout {

    @DubboReference(version = "1.0.0", group = "orders")
    private OrderService orders;

    public String buy(String sku) {
        return orders.place(sku);
    }
}
