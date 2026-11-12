package com.acme;

/** The Dubbo interface. Its implementation is in another class, and in a real
 *  deployment in another repository — which is the edge that matters. */
public interface OrderService {
    String find(String id);

    String place(String sku);
}
