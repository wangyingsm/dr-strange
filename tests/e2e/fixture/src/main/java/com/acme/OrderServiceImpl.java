package com.acme;

import org.apache.dubbo.config.annotation.DubboService;

/** The Dubbo provider: what actually answers the interface's calls. */
@DubboService(version = "1.0.0", group = "orders")
public class OrderServiceImpl implements OrderService {

    private final OrderMapper mapper;

    public OrderServiceImpl(OrderMapper mapper) {
        this.mapper = mapper;
    }

    @Override
    public String find(String id) {
        return mapper.findById(id);
    }

    @Override
    public String place(String sku) {
        mapper.insert(sku);
        return sku;
    }
}
